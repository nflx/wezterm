use crate::tmux::{RefTmuxRemotePane, TmuxCmdQueue, TmuxDomainState};
use crate::tmux_commands::{KillPane, Resize, SendKeys};
use crate::{DomainId, Mux};
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty};
use std::io::{Read, Write};
use std::sync::Arc;

/// A local tmux pane(tab) based on a tmux pty
#[derive(Debug)]
pub(crate) struct TmuxPty {
    pub domain_id: DomainId,
    pub master_pane: RefTmuxRemotePane,
    pub reader: FileDescriptor,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

struct TmuxPtyWriter {
    domain_id: DomainId,
    master_pane: RefTmuxRemotePane,
    cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

fn connection_state(domain_id: DomainId) -> Option<crate::tab::TmuxConnectionState> {
    Mux::try_get()
        .and_then(|mux| mux.get_domain(domain_id))
        .and_then(|domain| {
            domain
                .downcast_ref::<crate::tmux::TmuxDomain>()
                .map(|tmux| tmux.connection_state())
        })
}

fn ensure_connected(domain_id: DomainId) -> std::io::Result<()> {
    if connection_state(domain_id) == Some(crate::tab::TmuxConnectionState::Connected) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "tmux control connection is not ready; input was not sent",
        ))
    }
}

impl Write for TmuxPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        ensure_connected(self.domain_id)?;
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        log::trace!("pane:{}, content:{:?}", &pane_id, buf);
        let mut cmd_queue = self.cmd_queue.lock();
        cmd_queue.push_back(Box::new(SendKeys {
            pane: pane_id,
            keys: buf.to_vec(),
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Write for TmuxPty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        ensure_connected(self.domain_id)?;
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        log::trace!("pane:{}, content:{:?}", &pane_id, buf);
        let mut cmd_queue = self.cmd_queue.lock();
        cmd_queue.push_back(Box::new(SendKeys {
            pane: pane_id,
            keys: buf.to_vec(),
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TmuxChild {
    pub active_lock: Arc<(Mutex<bool>, Condvar)>,
    pub domain_id: DomainId,
    pub pane_id: termwiz::tmux_cc::TmuxPaneId,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

impl Child for TmuxChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        if *self.active_lock.0.lock() {
            Ok(Some(ExitStatus::with_exit_code(0)))
        } else {
            Ok(None)
        }
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        let &(ref lock, ref var) = &*self.active_lock;
        let mut released = lock.lock();
        while !*released {
            var.wait(&mut released);
        }
        return Ok(ExitStatus::with_exit_code(0));
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

#[derive(Clone, Debug)]
struct TmuxChildKiller {
    active_lock: Arc<(Mutex<bool>, Condvar)>,
    domain_id: DomainId,
    pane_id: termwiz::tmux_cc::TmuxPaneId,
    cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

fn kill_tmux_pane(
    active_lock: &Arc<(Mutex<bool>, Condvar)>,
    domain_id: DomainId,
    pane_id: termwiz::tmux_cc::TmuxPaneId,
    cmd_queue: &Arc<Mutex<TmuxCmdQueue>>,
) {
    let (lock, var) = &**active_lock;
    let mut exited = lock.lock();
    if *exited {
        return;
    }
    *exited = true;
    var.notify_all();
    drop(exited);
    cmd_queue.lock().push_back(Box::new(KillPane { pane_id }));
    if Mux::try_get().is_some() {
        TmuxDomainState::schedule_send_next_command(domain_id);
    }
}

impl ChildKiller for TmuxChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        kill_tmux_pane(
            &self.active_lock,
            self.domain_id,
            self.pane_id,
            &self.cmd_queue,
        );
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl ChildKiller for TmuxChild {
    fn kill(&mut self) -> std::io::Result<()> {
        kill_tmux_pane(
            &self.active_lock,
            self.domain_id,
            self.pane_id,
            &self.cmd_queue,
        );
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(TmuxChildKiller {
            active_lock: Arc::clone(&self.active_lock),
            domain_id: self.domain_id,
            pane_id: self.pane_id,
            cmd_queue: Arc::clone(&self.cmd_queue),
        })
    }
}

impl MasterPty for TmuxPty {
    fn resize(&self, size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        match connection_state(self.domain_id) {
            Some(crate::tab::TmuxConnectionState::Syncing)
            | Some(crate::tab::TmuxConnectionState::Connected) => {}
            _ => ensure_connected(self.domain_id)?,
        }
        let mut cmd_queue = self.cmd_queue.lock();
        let pane_id = self.master_pane.lock().pane_id;
        cmd_queue.retain(|command| command.resize_pane_id() != Some(pane_id));
        cmd_queue.push_back(Box::new(Resize { size, pane_id }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(())
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        let pane = self.master_pane.lock();
        Ok(portable_pty::PtySize {
            rows: pane.pane_height as u16,
            cols: pane.pane_width as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(self.reader.try_clone()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(TmuxPtyWriter {
            domain_id: self.domain_id,
            master_pane: self.master_pane.clone(),
            cmd_queue: self.cmd_queue.clone(),
        }))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        return None;
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_wait_and_kill_are_non_panicking_and_consistent() {
        let active_lock = Arc::new((Mutex::new(false), Condvar::new()));
        let mut child = TmuxChild {
            active_lock: Arc::clone(&active_lock),
            domain_id: 1,
            pane_id: 1,
            cmd_queue: Arc::new(Mutex::new(Default::default())),
        };

        assert!(child.try_wait().unwrap().is_none());
        let mut killer = child.clone_killer();
        killer.kill().unwrap();
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(child.wait().unwrap().exit_code(), 0);
    }
}

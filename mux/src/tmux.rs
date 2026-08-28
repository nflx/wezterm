use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{
    BreakPane, FocusPane, JoinPane, KillWindow, ListAllWindows, ListCommands, NewWindow,
    RenameWindow, SplitPane, TmuxCommand,
};
use crate::window::WindowId;
use crate::{Mux, MuxWindowBuilder};
use async_trait::async_trait;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::CommandBuilder;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum AttachState {
    Init,
    Done,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum State {
    WaitForInitialGuard,
    Idle,
    WaitingForResponse,
    Exit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TmuxRemotePane {
    // members for local
    pub local_pane_id: PaneId,
    pub output_write: FileDescriptor,
    pub active_lock: Arc<(Mutex<bool>, Condvar)>,
    // members sync with remote
    pub session_id: TmuxSessionId,
    pub window_id: TmuxWindowId,
    pub pane_id: TmuxPaneId,
    pub cursor_x: u64,
    pub cursor_y: u64,
    pub pane_width: u64,
    pub pane_height: u64,
    pub pane_left: u64,
    pub pane_top: u64,
}

pub(crate) type RefTmuxRemotePane = Arc<Mutex<TmuxRemotePane>>;

/// As a remote TmuxTab, keeping the TmuxPanes ID
/// within the remote tab.
#[allow(dead_code)]
pub(crate) struct TmuxTab {
    pub tab_id: TabId, // local tab ID
    pub tmux_window_id: TmuxWindowId,
    pub layout_csum: String,
    pub layout_tree: LayoutNode,
    pub panes: HashSet<TmuxPaneId>, // tmux panes within tmux window
}

pub(crate) type TmuxCmdQueue = VecDeque<Box<dyn TmuxCommand>>;

pub(crate) struct PendingReposition {
    pub operation_id: u64,
    pub source: TmuxPaneId,
    pub target: TmuxPaneId,
    pub source_window: TmuxWindowId,
    pub target_window: TmuxWindowId,
    pub accepted: bool,
    pub completion: promise::Promise<()>,
}

pub(crate) struct PendingBreakPane {
    pub operation_id: u64,
    pub source: TmuxPaneId,
    pub accepted_window: Option<TmuxWindowId>,
    pub completion: promise::Promise<TmuxWindowId>,
}

pub(crate) struct PendingRename {
    pub operation_id: u64,
    pub window_id: TmuxWindowId,
    pub title: String,
    pub accepted: bool,
    pub completion: promise::Promise<()>,
}

pub(crate) struct PendingFocus {
    pub operation_id: u64,
    pub pane_id: TmuxPaneId,
    pub accepted: bool,
    pub completion: promise::Promise<()>,
}

pub(crate) struct PendingSplit {
    pub operation_id: u64,
    pub remote_id: Option<TmuxPaneId>,
    pub completion: promise::Promise<TmuxPaneId>,
}

fn retain_unsent_commands(
    queue: &mut TmuxCmdQueue,
    command_in_flight: bool,
    mut retain: impl FnMut(&Box<dyn TmuxCommand>) -> bool,
) {
    let in_flight = command_in_flight.then(|| queue.pop_front()).flatten();
    queue.retain(|command| retain(command));
    if let Some(command) = in_flight {
        queue.push_front(command);
    }
}

pub(crate) struct TmuxDomainState {
    pub pane_id: Mutex<PaneId>, // ID of the control transport pane
    pub domain_id: DomainId,    // ID of TmuxDomain
    pub(crate) managed: bool,
    state: Mutex<State>,
    pub(crate) connection_state: Mutex<crate::tab::TmuxConnectionState>,
    next_operation_id: AtomicU64,
    next_reposition_operation_id: AtomicU64,
    next_break_operation_id: AtomicU64,
    next_rename_operation_id: AtomicU64,
    next_focus_operation_id: AtomicU64,
    next_split_operation_id: AtomicU64,
    in_flight_operation_id: AtomicU64,
    in_flight_responses_remaining: AtomicUsize,
    retry_requested: AtomicBool,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub(crate) pending_splits: Mutex<VecDeque<PendingSplit>>,
    pub(crate) pending_new_tabs: Mutex<VecDeque<promise::Promise<Arc<Tab>>>>,
    pub(crate) pending_kills: Mutex<HashMap<TmuxPaneId, promise::Promise<()>>>,
    pub(crate) pending_window_kills: Mutex<HashMap<TmuxWindowId, promise::Promise<()>>>,
    pub(crate) pending_repositions: Mutex<VecDeque<PendingReposition>>,
    pub(crate) pending_breaks: Mutex<VecDeque<PendingBreakPane>>,
    pub(crate) pending_renames: Mutex<VecDeque<PendingRename>>,
    pub(crate) pending_focus: Mutex<VecDeque<PendingFocus>>,
    pub(crate) pending_reposition_windows: Mutex<HashSet<TmuxWindowId>>,
    pub backlog: Mutex<HashMap<TmuxPaneId, Vec<u8>>>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

impl TmuxDomainState {
    fn ensure_connected(&self) -> anyhow::Result<()> {
        if *self.connection_state.lock() == crate::tab::TmuxConnectionState::Connected {
            Ok(())
        } else {
            anyhow::bail!("tmux control connection is not ready; operation was not queued")
        }
    }

    fn fail_pending_repositions(&self, reason: &str) {
        let operations: Vec<_> = self.pending_repositions.lock().drain(..).collect();
        self.pending_reposition_windows.lock().clear();
        for mut operation in operations {
            operation
                .completion
                .err(anyhow::anyhow!(reason.to_string()));
        }
    }

    fn fail_pending_breaks(&self, reason: &str) {
        let operations: Vec<_> = self.pending_breaks.lock().drain(..).collect();
        for mut operation in operations {
            operation
                .completion
                .err(anyhow::anyhow!(reason.to_string()));
        }
    }

    fn fail_pending_window_kills(&self, reason: &str) {
        let operations: Vec<_> = self.pending_window_kills.lock().drain().collect();
        for (_, mut completion) in operations {
            completion.err(anyhow::anyhow!(reason.to_string()));
        }
    }

    fn fail_pending_renames(&self, reason: &str) {
        let operations: Vec<_> = self.pending_renames.lock().drain(..).collect();
        for mut operation in operations {
            operation
                .completion
                .err(anyhow::anyhow!(reason.to_string()));
        }
    }

    fn fail_pending_focus(&self, reason: &str) {
        let operations: Vec<_> = self.pending_focus.lock().drain(..).collect();
        for mut operation in operations {
            operation
                .completion
                .err(anyhow::anyhow!(reason.to_string()));
        }
    }

    pub(crate) fn retain_pending_commands(
        &self,
        mut retain: impl FnMut(&Box<dyn TmuxCommand>) -> bool,
    ) {
        let waiting = self.in_flight_operation_id.load(Ordering::Acquire) != 0;
        let mut queue = self.cmd_queue.lock();
        retain_unsent_commands(&mut queue, waiting, |command| retain(command));
    }

    pub fn advance(&self, events: Box<Vec<Event>>) {
        for event in events.iter() {
            let state = *self.state.lock();
            log::debug!("tmux: {:?} in state {:?}", event, state);
            match event {
                // Tmux generic events
                Event::Guarded(response) => match state {
                    State::WaitForInitialGuard => {
                        *self.state.lock() = State::Idle;
                        *self.connection_state.lock() = crate::tab::TmuxConnectionState::Syncing;
                    }
                    State::WaitingForResponse => {
                        if self
                            .in_flight_responses_remaining
                            .fetch_sub(1, Ordering::AcqRel)
                            > 1
                        {
                            continue;
                        }
                        self.in_flight_operation_id.store(0, Ordering::Release);
                        let cmd = self.cmd_queue.as_ref().lock().pop_front();
                        *self.state.lock() = State::Idle;
                        if let Some(cmd) = cmd {
                            let domain_id = self.domain_id;
                            let response = response.clone();
                            promise::spawn::spawn_into_main_thread(async move {
                                if let Err(err) = cmd.process_result(domain_id, &response) {
                                    log::error!("Tmux processing command result error: {}", err);
                                }
                                TmuxDomainState::schedule_send_next_command(domain_id);
                            })
                            .detach();
                        }
                    }
                    State::Idle => {}
                    State::Exit => {}
                },

                // Tmux specific events
                Event::ConfigError { error } => {
                    // tmux config file error, not our fault, just log it and go
                    log::warn!("tmux configuration error: {error}");
                }
                Event::Exit { reason: _ } => {
                    *self.state.lock() = State::Exit;
                    *self.connection_state.lock() = crate::tab::TmuxConnectionState::Disconnected;
                    self.in_flight_responses_remaining
                        .store(0, Ordering::Release);
                    if !self.managed {
                        let mut pane_map = self.remote_panes.lock();
                        for (_, v) in pane_map.iter_mut() {
                            let remote_pane = v.lock();
                            let (lock, condvar) = &*remote_pane.active_lock;
                            let mut released = lock.lock();
                            *released = true;
                            condvar.notify_all();
                        }
                    }
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.clear();
                    drop(cmd_queue);
                    let pending: Vec<_> = self.pending_kills.lock().drain().collect();
                    for (_, mut completion) in pending {
                        completion.err(anyhow::anyhow!("tmux control transport disconnected"));
                    }
                    self.fail_pending_repositions("tmux control transport disconnected");
                    self.fail_pending_breaks("tmux control transport disconnected");
                    self.fail_pending_window_kills("tmux control transport disconnected");
                    self.fail_pending_renames("tmux control transport disconnected");
                    self.fail_pending_focus("tmux control transport disconnected");

                    // Force to quit the tmux mode
                    let pane_id = *self.pane_id.lock();
                    promise::spawn::spawn_into_main_thread_with_low_priority(async move {
                        if let Some(x) = Mux::get().get_pane(pane_id) {
                            let _ = write!(x.writer(), "\n\n");
                        }
                    })
                    .detach();

                    return;
                }
                Event::LayoutChange {
                    window: _,
                    layout: _,
                    visible_layout: _,
                    raw_flags: _,
                } => {
                    if let Some(session_id) = *self.tmux_session.lock() {
                        let mut cmd_queue = self.cmd_queue.lock();
                        if !cmd_queue
                            .iter()
                            .any(|command| command.is_full_window_snapshot())
                        {
                            cmd_queue.push_back(Box::new(ListAllWindows {
                                session_id,
                                window_id: None,
                            }));
                        }
                    }
                }
                Event::Output { pane, text } => {
                    let pane_map = self.remote_panes.lock();
                    if let Some(ref_pane) = pane_map.get(pane) {
                        let mut tmux_pane = ref_pane.lock();
                        if let Err(err) = tmux_pane.output_write.write_all(text) {
                            log::error!("Failed to write tmux data to output: {:#}", err);
                        }
                    } else {
                        // the output may come early then pane is ready, in this case we
                        // backlog it
                        self.backlog.lock().insert(*pane, text.to_vec());
                        log::debug!("Tmux pane {} havn't been attached", pane);
                    }
                }
                Event::SessionChanged { session, name: _ } => {
                    *self.tmux_session.lock() = Some(*session);
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.push_back(Box::new(ListCommands));

                    self.subscribe_notification();
                    log::info!("tmux session changed:{}", session);
                }
                Event::WindowAdd { window } => {
                    // Only handle the new tab, the first empty window handled by sync_window_state
                    if !self.gui_window.lock().is_none() {
                        if let Some(session) = *self.tmux_session.lock() {
                            let mut cmd_queue = self.cmd_queue.as_ref().lock();
                            cmd_queue.push_back(Box::new(ListAllWindows {
                                session_id: session,
                                window_id: Some(*window),
                            }));
                            log::info!("tmux window add: {}:{}", session, window);
                        }
                    }
                }
                Event::WindowClose { window: _ } => {
                    // A window can disappear because its final pane moved to a
                    // surviving window.  Removing it directly from this early
                    // notification deregisters that still-live pane before we
                    // know its new owner.  Let the complete snapshot perform
                    // the atomic ownership rebind and detached-window removal.
                    if let Some(session_id) = *self.tmux_session.lock() {
                        let mut queue = self.cmd_queue.lock();
                        if !queue
                            .iter()
                            .any(|command| command.is_full_window_snapshot())
                        {
                            queue.push_front(Box::new(ListAllWindows {
                                session_id,
                                window_id: None,
                            }));
                        }
                    }
                }
                Event::WindowPaneChanged { window, pane } => {
                    // The tmux 2.7 WindowPaneChanged event comes early than WindowAdd, we need to
                    // skip it
                    if !self.check_window_attached(*window) {
                        continue;
                    }

                    // Split pane
                    if !self.check_pane_attached(*window, *pane) {
                        if !self.pending_splits.lock().is_empty() {
                            if let Some(session_id) = *self.tmux_session.lock() {
                                let mut cmd_queue = self.cmd_queue.lock();
                                if !cmd_queue
                                    .iter()
                                    .any(|command| command.is_full_window_snapshot())
                                {
                                    cmd_queue.push_back(Box::new(ListAllWindows {
                                        session_id,
                                        window_id: None,
                                    }));
                                }
                            }
                        }
                    }
                    log::info!("tmux window pane changed: {}:{}", window, pane);
                }
                Event::WindowRenamed { window, name } => {
                    let gui_tabs = self.gui_tabs.lock();
                    if let Some(x) = gui_tabs.get(&window) {
                        let mux = Mux::get();
                        if let Some(tab) = mux.get_tab(x.tab_id) {
                            tab.set_title(&format!("{}", name));
                        }
                    }
                }
                Event::UnlinkedWindowClose { window: _ } => {
                    // Like WindowClose, this notification can arrive before we
                    // have authoritative information about where the panes in
                    // the unlinked window went.  In particular, killing the
                    // final pane in a window emits this event; eagerly removing
                    // the local tab here prevents the complete snapshot from
                    // observing the removed pane and completing its pending
                    // kill operation.  Always defer removal and ownership
                    // changes to the atomic snapshot reconciler.
                    if let Some(session_id) = *self.tmux_session.lock() {
                        let mut queue = self.cmd_queue.lock();
                        if !queue
                            .iter()
                            .any(|command| command.is_full_window_snapshot())
                        {
                            queue.push_front(Box::new(ListAllWindows {
                                session_id,
                                window_id: None,
                            }));
                        }
                    }
                }
                _ => {}
            }
        }

        // send pending commands to tmux
        let idle = *self.state.lock() == State::Idle;
        let has_pending_commands = !self.cmd_queue.lock().is_empty();
        if idle && has_pending_commands {
            TmuxDomainState::schedule_send_next_command(self.domain_id);
        }
    }

    /// send next command at the front of cmd_queue.
    /// must be called inside main thread
    fn send_next_command(&self) {
        if *self.state.lock() != State::Idle {
            return;
        }
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        while let Some(first) = cmd_queue.front() {
            let cmd = first.get_command(self.domain_id);
            if cmd.is_empty() {
                cmd_queue.pop_front();
                continue;
            }
            log::debug!("sending cmd {:?}", cmd);
            self.in_flight_responses_remaining
                .store(first.guarded_response_count(), Ordering::Release);
            let mux = Mux::get();
            if let Some(pane) = mux.get_pane(*self.pane_id.lock()) {
                let mut writer = pane.writer();
                let fault_match = std::env::var("WEZTERM_TMUX_TEST_FAULT_MATCH").ok();
                let fault_mode = std::env::var("WEZTERM_TMUX_TEST_FAULT_MODE").ok();
                let fault_armed = std::env::var_os("WEZTERM_TMUX_TEST_FAULT_ARM_FILE")
                    .is_none_or(|path| std::path::Path::new(&path).exists());
                let inject = fault_match.as_deref().is_some_and(|needle| {
                    fault_armed && !needle.is_empty() && cmd.contains(needle)
                });
                match (inject, fault_mode.as_deref()) {
                    (true, Some("timeout")) => {
                        log::warn!("injecting tmux command timeout for {cmd:?}");
                    }
                    (true, Some("reject")) => {
                        log::warn!("injecting tmux command rejection for {cmd:?}");
                        for _ in 0..first.guarded_response_count() {
                            let _ = writeln!(writer, "__wezterm_injected_command_failure__");
                        }
                    }
                    _ => {
                        let _ = write!(writer, "{}", cmd);
                    }
                }
            }
            *self.state.lock() = State::WaitingForResponse;
            let operation_id = self.next_operation_id.fetch_add(1, Ordering::Relaxed) + 1;
            self.in_flight_operation_id
                .store(operation_id, Ordering::Release);
            let domain_id = self.domain_id;
            let timeout = std::env::var("WEZTERM_TMUX_TEST_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or_else(|| Duration::from_secs(10));
            std::thread::spawn(move || {
                std::thread::sleep(timeout);
                promise::spawn::spawn_into_main_thread(async move {
                    let mux = Mux::get();
                    let Some(domain) = mux.get_domain(domain_id) else {
                        return;
                    };
                    let Some(tmux) = domain.downcast_ref::<TmuxDomain>() else {
                        return;
                    };
                    tmux.inner.command_timed_out(operation_id);
                })
                .detach();
            });
            break;
        }
    }

    fn command_timed_out(&self, operation_id: u64) {
        if self
            .in_flight_operation_id
            .compare_exchange(operation_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if *self.state.lock() != State::WaitingForResponse {
            return;
        }
        *self.state.lock() = State::Exit;
        *self.connection_state.lock() = crate::tab::TmuxConnectionState::Disconnected;
        self.in_flight_responses_remaining
            .store(0, Ordering::Release);
        if let Some(command) = self.cmd_queue.lock().pop_front() {
            log::error!("tmux operation {operation_id} timed out: {command:?}");
            if let Err(err) = command.process_timeout(self.domain_id) {
                log::error!("{err:#}");
            }
        }
        self.cmd_queue.lock().clear();
        let pending: Vec<_> = self.pending_kills.lock().drain().collect();
        for (_, mut completion) in pending {
            completion.err(anyhow::anyhow!("tmux command actor timed out"));
        }
        self.fail_pending_repositions("tmux command actor timed out");
        self.fail_pending_breaks("tmux command actor timed out");
        self.fail_pending_window_kills("tmux command actor timed out");
        self.fail_pending_renames("tmux command actor timed out");
        self.fail_pending_focus("tmux command actor timed out");
        if let Some(transport) = Mux::get().get_pane(*self.pane_id.lock()) {
            transport.kill();
        }
    }

    /// schedule a `send_next_command` into main thread
    pub fn schedule_send_next_command(domain_id: usize) {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            if let Some(domain) = mux.get_domain(domain_id) {
                if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                    tmux_domain.send_next_command();
                }
            }
        })
        .detach();
    }

    /// create a standalone window for tmux tabs
    pub fn create_gui_window(&self) {
        if self.gui_window.lock().is_none() {
            let mux = Mux::get();
            let window_builder = if let Some((_domain, window_id, control_tab_id)) =
                mux.resolve_pane_id(*self.pane_id.lock())
            {
                // The pane that carries the tmux -CC protocol is transport,
                // not a user-facing terminal. Keep its tab and pane registered
                // in the mux so that the child and reader remain alive, but
                // detach the tab from the visible window. The Activity held by
                // the builder prevents the temporarily empty window from being
                // pruned before the tmux windows are reconstructed below.
                let activity = Activity::new();
                if let Some(mut window) = mux.get_window_mut(window_id) {
                    window.remove_by_id(control_tab_id);
                }
                MuxWindowBuilder {
                    window_id,
                    activity: Some(activity),
                    notified: false,
                }
            } else {
                mux.new_empty_window(
                    None, /* TODO: pass session here */
                    None, /* position */
                )
            };

            log::info!("Tmux create window id {}", window_builder.window_id);
            {
                let mut window_id = self.gui_window.lock();
                *window_id = Some(window_builder); // keep the builder so it won't be purged
            }
        };
    }

    /// create a tmux window
    pub fn create_tmux_window(&self, command: Option<Vec<String>>, cwd: Option<String>) {
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        cmd_queue.push_back(Box::new(NewWindow { command, cwd }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
    }

    async fn spawn_tmux_window(
        &self,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<Tab>> {
        self.ensure_connected()?;
        let builder_cwd = command
            .as_ref()
            .and_then(|command| command.get_cwd())
            .map(|cwd| {
                cwd.to_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("tmux cwd contains non-UTF-8 data"))
            })
            .transpose()?;
        let command = command
            .map(|command| {
                command
                    .get_argv()
                    .iter()
                    .map(|arg| {
                        arg.to_str()
                            .map(ToOwned::to_owned)
                            .ok_or_else(|| anyhow::anyhow!("tmux command contains non-UTF-8 data"))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .transpose()?;
        let cwd = command_dir.or(builder_cwd);
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux new-window completion"))?;
        self.pending_new_tabs.lock().push_back(completion);
        self.create_tmux_window(command, cwd);
        future.await
    }

    /// split the tmux pane
    pub fn split_tmux_pane(
        &self,
        _tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<u64> {
        self.ensure_connected()?;
        let tmux_pane_id = self
            .remote_panes
            .lock()
            .iter()
            .find(|(_, ref_pane)| ref_pane.lock().local_pane_id == pane_id)
            .map(|p| p.1.lock().pane_id);

        if let Some(id) = tmux_pane_id {
            let operation_id = self.next_split_operation_id.fetch_add(1, Ordering::Relaxed) + 1;
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            cmd_queue.push_back(Box::new(SplitPane {
                operation_id,
                pane_id: id,
                direction: split_request.direction,
            }));
            TmuxDomainState::schedule_send_next_command(self.domain_id);
            return Ok(operation_id);
        } else {
            anyhow::bail!("Could not find the tmux pane peer for local pane: {pane_id}");
        }
    }

    pub async fn reposition_tmux_pane(
        &self,
        pane_id: PaneId,
        target_pane_id: PaneId,
        request: SplitRequest,
    ) -> anyhow::Result<()> {
        self.ensure_connected()?;
        let (source, source_window, target, target_window) = {
            let pane_map = self.remote_panes.lock();
            let (source, source_window) = pane_map
                .values()
                .find(|pane| pane.lock().local_pane_id == pane_id)
                .map(|pane| {
                    let pane = pane.lock();
                    (pane.pane_id, pane.window_id)
                })
                .ok_or_else(|| anyhow::anyhow!("no tmux pane for local pane {pane_id}"))?;
            let (target, target_window) = pane_map
                .values()
                .find(|pane| pane.lock().local_pane_id == target_pane_id)
                .map(|pane| {
                    let pane = pane.lock();
                    (pane.pane_id, pane.window_id)
                })
                .ok_or_else(|| anyhow::anyhow!("no tmux pane for local target {target_pane_id}"))?;
            (source, source_window, target, target_window)
        };

        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux reposition completion"))?;
        let operation_id = self
            .next_reposition_operation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.pending_reposition_windows.lock().insert(source_window);
        self.pending_repositions
            .lock()
            .push_back(PendingReposition {
                operation_id,
                source,
                target,
                source_window,
                target_window,
                accepted: false,
                completion,
            });
        self.cmd_queue.lock().push_back(Box::new(JoinPane {
            operation_id,
            source,
            source_window,
            target,
            request,
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        future.await
    }

    pub(crate) fn pane_reposition_failed(&self, operation_id: u64, source_window: TmuxWindowId) {
        self.pending_reposition_windows
            .lock()
            .remove(&source_window);
        let completion = {
            let mut pending = self.pending_repositions.lock();
            pending
                .iter()
                .position(|operation| operation.operation_id == operation_id)
                .and_then(|index| pending.remove(index))
                .map(|operation| operation.completion)
        };
        if let Some(mut completion) = completion {
            completion.err(anyhow::anyhow!("tmux pane reposition failed"));
        }
    }

    async fn break_pane_to_new_tab(&self, local_pane_id: PaneId) -> anyhow::Result<TmuxWindowId> {
        self.ensure_connected()?;
        let source = self
            .remote_panes
            .lock()
            .iter()
            .find_map(|(remote_id, pane)| {
                (pane.lock().local_pane_id == local_pane_id).then_some(*remote_id)
            })
            .ok_or_else(|| anyhow::anyhow!("no tmux pane for local pane {local_pane_id}"))?;
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux break-pane completion"))?;
        let operation_id = self.next_break_operation_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending_breaks.lock().push_back(PendingBreakPane {
            operation_id,
            source,
            accepted_window: None,
            completion,
        });
        self.cmd_queue.lock().push_back(Box::new(BreakPane {
            operation_id,
            source,
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        future.await
    }

    pub(crate) fn break_pane_failed(&self, operation_id: u64) {
        let completion = {
            let mut pending = self.pending_breaks.lock();
            pending
                .iter()
                .position(|operation| operation.operation_id == operation_id)
                .and_then(|index| pending.remove(index))
                .map(|operation| operation.completion)
        };
        if let Some(mut completion) = completion {
            completion.err(anyhow::anyhow!("tmux break-pane failed"));
        }
    }
}

#[cfg(test)]
mod command_queue_tests {
    use super::*;
    use termwiz::tmux_cc::Guarded;

    #[derive(Debug)]
    struct TestCommand(u64);

    impl TmuxCommand for TestCommand {
        fn get_command(&self, _domain_id: DomainId) -> String {
            self.0.to_string()
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            Ok(())
        }

        fn resize_pane_id(&self) -> Option<TmuxPaneId> {
            Some(self.0)
        }
    }

    #[test]
    fn coalescing_never_removes_the_in_flight_command() {
        let mut queue: TmuxCmdQueue = VecDeque::from([
            Box::new(TestCommand(1)) as Box<dyn TmuxCommand>,
            Box::new(TestCommand(1)),
            Box::new(TestCommand(2)),
        ]);

        retain_unsent_commands(&mut queue, true, |command| {
            command.resize_pane_id() != Some(1)
        });

        let pane_ids: Vec<_> = queue
            .iter()
            .map(|command| command.resize_pane_id().unwrap())
            .collect();
        assert_eq!(pane_ids, vec![1, 2]);
    }

    #[test]
    fn domain_state_tracks_only_a_completed_connection() {
        use crate::tab::TmuxConnectionState::*;

        assert_eq!(
            TmuxDomain::domain_state_for_connection(Connected),
            DomainState::Attached
        );
        for state in [Connecting, Syncing, Reconnecting, Disconnected] {
            assert_eq!(
                TmuxDomain::domain_state_for_connection(state),
                DomainState::Detached
            );
        }
    }
}

impl TmuxDomain {
    pub fn new(pane_id: PaneId) -> Self {
        let domain_id = alloc_domain_id();
        let cmd_queue = VecDeque::new();
        let inner = Arc::new(TmuxDomainState {
            domain_id,
            pane_id: Mutex::new(pane_id),
            // parser,
            managed: config::configuration().tmux_control.is_some(),
            state: Mutex::new(State::WaitForInitialGuard),
            connection_state: Mutex::new(crate::tab::TmuxConnectionState::Connecting),
            next_operation_id: AtomicU64::new(0),
            next_reposition_operation_id: AtomicU64::new(0),
            next_break_operation_id: AtomicU64::new(0),
            next_rename_operation_id: AtomicU64::new(0),
            next_focus_operation_id: AtomicU64::new(0),
            next_split_operation_id: AtomicU64::new(0),
            in_flight_operation_id: AtomicU64::new(0),
            in_flight_responses_remaining: AtomicUsize::new(0),
            retry_requested: AtomicBool::new(false),
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            tmux_session: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            pending_splits: Mutex::new(VecDeque::default()),
            pending_new_tabs: Mutex::new(VecDeque::default()),
            pending_kills: Mutex::new(HashMap::default()),
            pending_window_kills: Mutex::new(HashMap::default()),
            pending_repositions: Mutex::new(VecDeque::default()),
            pending_breaks: Mutex::new(VecDeque::default()),
            pending_renames: Mutex::new(VecDeque::default()),
            pending_focus: Mutex::new(VecDeque::default()),
            pending_reposition_windows: Mutex::new(HashSet::default()),
            backlog: Mutex::new(HashMap::default()),
        });

        Self { inner }
    }

    fn send_next_command(&self) {
        self.inner.send_next_command();
    }

    pub fn connection_state(&self) -> crate::tab::TmuxConnectionState {
        *self.inner.connection_state.lock()
    }

    fn domain_state_for_connection(
        connection_state: crate::tab::TmuxConnectionState,
    ) -> DomainState {
        if connection_state == crate::tab::TmuxConnectionState::Connected {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }

    fn ensure_connected(&self) -> anyhow::Result<()> {
        self.inner.ensure_connected()
    }

    pub fn is_managed(&self) -> bool {
        self.inner.managed
    }

    pub(crate) fn reconnect_transport(&self, pane_id: PaneId) {
        *self.inner.pane_id.lock() = pane_id;
        *self.inner.state.lock() = State::WaitForInitialGuard;
        *self.inner.connection_state.lock() = crate::tab::TmuxConnectionState::Reconnecting;
        *self.inner.attach_state.lock() = AttachState::Init;
        self.inner
            .in_flight_operation_id
            .store(0, Ordering::Release);
        self.inner
            .in_flight_responses_remaining
            .store(0, Ordering::Release);
        self.inner.cmd_queue.lock().clear();
        let pending: Vec<_> = self.inner.pending_kills.lock().drain().collect();
        for (_, mut completion) in pending {
            completion.err(anyhow::anyhow!("tmux control transport reconnected"));
        }
        self.inner
            .fail_pending_repositions("tmux control transport reconnected");
        self.inner
            .fail_pending_breaks("tmux control transport reconnected");
        self.inner
            .fail_pending_window_kills("tmux control transport reconnected");
        self.inner
            .fail_pending_renames("tmux control transport reconnected");
        self.inner
            .fail_pending_focus("tmux control transport reconnected");
    }

    pub(crate) fn transport_disconnected(&self) {
        *self.inner.state.lock() = State::Exit;
        *self.inner.connection_state.lock() = crate::tab::TmuxConnectionState::Disconnected;
        self.inner
            .in_flight_operation_id
            .store(0, Ordering::Release);
        self.inner
            .in_flight_responses_remaining
            .store(0, Ordering::Release);
        self.inner
            .fail_pending_repositions("tmux control transport disconnected");
        self.inner
            .fail_pending_breaks("tmux control transport disconnected");
        self.inner
            .fail_pending_window_kills("tmux control transport disconnected");
        self.inner
            .fail_pending_renames("tmux control transport disconnected");
        self.inner
            .fail_pending_focus("tmux control transport disconnected");
    }

    pub fn mark_reconnecting(&self) {
        *self.inner.connection_state.lock() = crate::tab::TmuxConnectionState::Reconnecting;
        let mux = Mux::get();
        for tab_id in self.inner.gui_tabs.lock().values().map(|tab| tab.tab_id) {
            mux.notify(crate::MuxNotification::TabResized(tab_id));
        }
    }

    pub fn request_retry(&self) {
        self.inner.retry_requested.store(true, Ordering::Release);
    }

    pub fn take_retry_request(&self) -> bool {
        self.inner.retry_requested.swap(false, Ordering::AcqRel)
    }

    pub async fn rename_tab(&self, tab_id: TabId, title: String) -> anyhow::Result<()> {
        self.ensure_connected()?;
        let window_id = self
            .inner
            .gui_tabs
            .lock()
            .values()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.tmux_window_id)
            .ok_or_else(|| anyhow::anyhow!("no tmux window for tab {tab_id}"))?;
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux rename completion"))?;
        let operation_id = self
            .inner
            .next_rename_operation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.inner.pending_renames.lock().push_back(PendingRename {
            operation_id,
            window_id,
            title: title.clone(),
            accepted: false,
            completion,
        });
        self.inner
            .cmd_queue
            .lock()
            .push_back(Box::new(RenameWindow {
                operation_id,
                window_id,
                title,
            }));
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        future.await
    }

    pub async fn focus_pane(&self, local_pane_id: PaneId) -> anyhow::Result<()> {
        self.ensure_connected()?;
        let (pane_id, window_id) = {
            let panes = self.inner.remote_panes.lock();
            panes
                .values()
                .find(|pane| pane.lock().local_pane_id == local_pane_id)
                .map(|pane| {
                    let pane = pane.lock();
                    (pane.pane_id, pane.window_id)
                })
                .ok_or_else(|| anyhow::anyhow!("no tmux pane for local pane {local_pane_id}"))?
        };
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux focus completion"))?;
        let operation_id = self
            .inner
            .next_focus_operation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.inner.pending_focus.lock().push_back(PendingFocus {
            operation_id,
            pane_id,
            accepted: false,
            completion,
        });
        self.inner.cmd_queue.lock().push_back(Box::new(FocusPane {
            operation_id,
            pane_id,
            window_id,
        }));
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        future.await
    }

    pub async fn close_tab(&self, tab_id: TabId) -> anyhow::Result<()> {
        self.ensure_connected()?;
        let window_id = self
            .inner
            .gui_tabs
            .lock()
            .values()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.tmux_window_id)
            .ok_or_else(|| anyhow::anyhow!("no tmux window for tab {tab_id}"))?;
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux window-close completion"))?;
        self.inner
            .pending_window_kills
            .lock()
            .insert(window_id, completion);
        self.inner
            .cmd_queue
            .lock()
            .push_back(Box::new(KillWindow { window_id }));
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        future.await
    }

    pub async fn kill_pane(&self, local_pane_id: PaneId) -> anyhow::Result<()> {
        log::info!("tmux transactional close requested for local pane {local_pane_id}");
        self.ensure_connected()?;
        let remote_pane_id = {
            self.inner
                .remote_panes
                .lock()
                .iter()
                .find_map(|(remote_id, pane)| {
                    (pane.lock().local_pane_id == local_pane_id).then_some(*remote_id)
                })
                .ok_or_else(|| anyhow::anyhow!("no tmux pane for local pane {local_pane_id}"))?
        };
        log::info!(
            "tmux transactional close mapped local pane {local_pane_id} to %{remote_pane_id}"
        );
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux kill completion"))?;
        self.inner
            .pending_kills
            .lock()
            .insert(remote_pane_id, completion);
        self.inner
            .retain_pending_commands(|command| !command.is_stale_for_killed_pane(remote_pane_id));
        {
            let mut queue = self.inner.cmd_queue.lock();
            queue.push_back(Box::new(crate::tmux_commands::KillPane {
                pane_id: remote_pane_id,
            }));
        }
        log::info!("tmux transactional close queued kill-pane %{remote_pane_id}");
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        future.await
    }
}

#[async_trait(?Send)]
impl Domain for TmuxDomain {
    async fn spawn(
        &self,
        _size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        _window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        self.inner.spawn_tmux_window(command, command_dir).await
    }

    async fn split_pane(
        &self,
        source: SplitSource,
        tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        if let SplitSource::MovePane(source_pane_id) = source {
            self.inner
                .reposition_tmux_pane(source_pane_id, pane_id, split_request)
                .await?;
            return Mux::get()
                .get_pane(source_pane_id)
                .ok_or_else(|| anyhow::anyhow!("moved pane {source_pane_id} disappeared"));
        }
        let mut promise = promise::Promise::new();
        if let Some(future) = promise.get_future() {
            {
                let mut pending_splits = self.inner.pending_splits.lock();
                let operation_id = self.inner.split_tmux_pane(tab, pane_id, split_request)?;
                pending_splits.push_back(PendingSplit {
                    operation_id,
                    remote_id: None,
                    completion: promise,
                });
            }

            if let Ok(id) = future.await {
                let local_pane_id = self
                    .inner
                    .remote_panes
                    .lock()
                    .get(&id)
                    .map(|pane| pane.lock().local_pane_id)
                    .ok_or_else(|| anyhow::anyhow!("reconciled tmux pane %{id} disappeared"))?;
                return Mux::get().get_pane(local_pane_id).ok_or_else(|| {
                    anyhow::anyhow!("reconciled local pane {local_pane_id} disappeared")
                });
            }
        }

        anyhow::bail!("Split_pane failed");
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        self.inner
            .spawn_tmux_window(command, command_dir)
            .await?
            .get_active_pane()
            .ok_or_else(|| anyhow::anyhow!("reconciled tmux window has no active pane"))
    }

    async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<(Arc<Tab>, WindowId)>> {
        if workspace_for_new_window.is_some() {
            anyhow::bail!("moving a tmux pane to a separate workspace is not supported");
        }
        let managed_window = self
            .inner
            .gui_window
            .lock()
            .as_ref()
            .map(|window| window.window_id)
            .ok_or_else(|| anyhow::anyhow!("managed tmux mux window is unavailable"))?;
        if window_id.is_some_and(|requested| requested != managed_window) {
            anyhow::bail!("moving a tmux pane to a separate GUI window is not supported");
        }
        let tmux_window_id = self.inner.break_pane_to_new_tab(pane_id).await?;
        let tab_id = self
            .inner
            .gui_tabs
            .lock()
            .get(&tmux_window_id)
            .map(|tab| tab.tab_id)
            .ok_or_else(|| {
                anyhow::anyhow!("reconciled tmux window @{tmux_window_id} disappeared")
            })?;
        let tab = Mux::get()
            .get_tab(tab_id)
            .ok_or_else(|| anyhow::anyhow!("reconciled mux tab {tab_id} disappeared"))?;
        Ok(Some((tab, managed_window)))
    }

    fn domain_id(&self) -> DomainId {
        self.inner.domain_id
    }

    fn domain_name(&self) -> &str {
        "tmux"
    }

    async fn attach(&self, _window_id: Option<crate::WindowId>) -> anyhow::Result<()> {
        if self.state() == DomainState::Attached {
            return Ok(());
        }
        if !self.is_managed() {
            anyhow::bail!(
                "a manually launched tmux control domain cannot be reattached without a new tmux -CC transport"
            );
        }

        self.request_retry();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if self.state() == DomainState::Attached {
                return Ok(());
            }
            smol::Timer::after(std::time::Duration::from_millis(50)).await;
        }
        anyhow::bail!("timed out waiting for the managed tmux domain to reconnect")
    }

    fn detachable(&self) -> bool {
        !self.is_managed()
    }

    fn detach(&self) -> anyhow::Result<()> {
        if self.is_managed() {
            anyhow::bail!(
                "the managed tmux domain is supervised and cannot be detached explicitly"
            );
        }
        if self.state() == DomainState::Detached {
            return Ok(());
        }
        let pane_id = *self.inner.pane_id.lock();
        let transport = Mux::get()
            .get_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("tmux control transport pane {pane_id} disappeared"))?;
        write!(transport.writer(), "detach-client\n")?;
        Ok(())
    }

    fn state(&self) -> DomainState {
        Self::domain_state_for_connection(self.connection_state())
    }
}

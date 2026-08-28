use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{
    JoinPane, KillWindow, ListAllWindows, ListCommands, NewWindow, RenameWindow, SplitPane,
    TmuxCommand,
};
use crate::window::WindowId;
use crate::{Mux, MuxWindowBuilder};
use async_trait::async_trait;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::CommandBuilder;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
pub(crate) struct TmuxDomainState {
    pub pane_id: Mutex<PaneId>, // ID of the control transport pane
    pub domain_id: DomainId,    // ID of TmuxDomain
    managed: bool,
    state: Mutex<State>,
    pub(crate) connection_state: Mutex<crate::tab::TmuxConnectionState>,
    next_operation_id: AtomicU64,
    in_flight_operation_id: AtomicU64,
    retry_requested: AtomicBool,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub(crate) pending_splits: Mutex<VecDeque<promise::Promise<TmuxPaneId>>>,
    pub(crate) pending_split_pane_ids: Mutex<VecDeque<TmuxPaneId>>,
    pub(crate) pending_new_tabs: Mutex<VecDeque<promise::Promise<Arc<Tab>>>>,
    pending_reposition_windows: Mutex<HashSet<TmuxWindowId>>,
    pub backlog: Mutex<HashMap<TmuxPaneId, Vec<u8>>>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

impl TmuxDomainState {
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
                        self.in_flight_operation_id.store(0, Ordering::Release);
                        let mut cmd_queue = self.cmd_queue.as_ref().lock();
                        if let Some(cmd) = cmd_queue.pop_front() {
                            let domain_id = self.domain_id;
                            *self.state.lock() = State::Idle;
                            let resp = response.clone();
                            promise::spawn::spawn_into_main_thread(async move {
                                if let Err(err) = cmd.process_result(domain_id, &resp) {
                                    log::error!("Tmux processing command result error: {}", err);
                                }
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
                Event::WindowClose { window } => {
                    if self.pending_reposition_windows.lock().contains(window) {
                        continue;
                    }
                    let _ = self.remove_detached_window(*window);
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
                            let mut pending_ids = self.pending_split_pane_ids.lock();
                            if !pending_ids.contains(pane) {
                                pending_ids.push_back(*pane);
                            }
                            drop(pending_ids);
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
                Event::UnlinkedWindowClose { window } => {
                    if self.pending_reposition_windows.lock().contains(window) {
                        continue;
                    }
                    let _ = self.remove_detached_window(*window);
                }
                _ => {}
            }
        }

        // send pending commands to tmux
        let cmd_queue = self.cmd_queue.as_ref().lock();
        if *self.state.lock() == State::Idle && !cmd_queue.is_empty() {
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
            let mux = Mux::get();
            if let Some(pane) = mux.get_pane(*self.pane_id.lock()) {
                let mut writer = pane.writer();
                let _ = write!(writer, "{}", cmd);
            }
            *self.state.lock() = State::WaitingForResponse;
            let operation_id = self.next_operation_id.fetch_add(1, Ordering::Relaxed) + 1;
            self.in_flight_operation_id
                .store(operation_id, Ordering::Release);
            let domain_id = self.domain_id;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(10));
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
        if let Some(command) = self.cmd_queue.lock().pop_front() {
            if let Err(err) = command.process_timeout(self.domain_id) {
                log::error!("{err:#}");
            }
        }
        self.cmd_queue.lock().clear();
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
    pub fn create_tmux_window(&self) {
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        cmd_queue.push_back(Box::new(NewWindow));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
    }

    /// split the tmux pane
    pub fn split_tmux_pane(
        &self,
        _tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<()> {
        let tmux_pane_id = self
            .remote_panes
            .lock()
            .iter()
            .find(|(_, ref_pane)| ref_pane.lock().local_pane_id == pane_id)
            .map(|p| p.1.lock().pane_id);

        if let Some(id) = tmux_pane_id {
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            cmd_queue.push_back(Box::new(SplitPane {
                pane_id: id,
                direction: split_request.direction,
            }));
            TmuxDomainState::schedule_send_next_command(self.domain_id);
            return Ok(());
        } else {
            anyhow::bail!("Could not find the tmux pane peer for local pane: {pane_id}");
        }
    }

    pub fn reposition_tmux_pane(
        &self,
        pane_id: PaneId,
        target_pane_id: PaneId,
        request: SplitRequest,
    ) -> anyhow::Result<()> {
        let pane_map = self.remote_panes.lock();
        let (source, source_window) = pane_map
            .values()
            .find(|pane| pane.lock().local_pane_id == pane_id)
            .map(|pane| {
                let pane = pane.lock();
                (pane.pane_id, pane.window_id)
            })
            .ok_or_else(|| anyhow::anyhow!("no tmux pane for local pane {pane_id}"))?;
        let target = pane_map
            .values()
            .find(|pane| pane.lock().local_pane_id == target_pane_id)
            .map(|pane| pane.lock().pane_id)
            .ok_or_else(|| anyhow::anyhow!("no tmux pane for local target {target_pane_id}"))?;
        drop(pane_map);

        self.pending_reposition_windows.lock().insert(source_window);
        self.cmd_queue.lock().push_back(Box::new(JoinPane {
            pane_id,
            target_pane_id,
            source,
            source_window,
            target,
            request,
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(())
    }

    pub(crate) fn pane_repositioned(
        &self,
        pane_id: PaneId,
        target_pane_id: PaneId,
        request: SplitRequest,
        source_window: TmuxWindowId,
    ) -> anyhow::Result<()> {
        self.pending_reposition_windows
            .lock()
            .remove(&source_window);
        let mux = Mux::get();
        mux.reposition_pane_locally(pane_id, target_pane_id, request)?;

        let (_, _, target_tab_id) = mux
            .resolve_pane_id(target_pane_id)
            .ok_or_else(|| anyhow::anyhow!("target pane disappeared after tmux move"))?;
        let target_window_id = self
            .gui_tabs
            .lock()
            .values()
            .find(|tab| tab.tab_id == target_tab_id)
            .map(|tab| tab.tmux_window_id)
            .ok_or_else(|| anyhow::anyhow!("target tmux window disappeared after move"))?;

        let pane_map = self.remote_panes.lock();
        let remote_pane_id = pane_map
            .values()
            .find(|pane| pane.lock().local_pane_id == pane_id)
            .map(|pane| pane.lock().pane_id)
            .ok_or_else(|| anyhow::anyhow!("moved tmux pane mapping disappeared"))?;
        if let Some(pane) = pane_map.get(&remote_pane_id) {
            pane.lock().window_id = target_window_id;
        }
        drop(pane_map);

        let mut tabs = self.gui_tabs.lock();
        for tab in tabs.values_mut() {
            tab.panes.remove(&remote_pane_id);
        }
        if let Some(tab) = tabs.get_mut(&target_window_id) {
            tab.panes.insert(remote_pane_id);
        }
        Ok(())
    }

    pub(crate) fn pane_reposition_failed(&self, source_window: TmuxWindowId) {
        self.pending_reposition_windows
            .lock()
            .remove(&source_window);
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
            in_flight_operation_id: AtomicU64::new(0),
            retry_requested: AtomicBool::new(false),
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            tmux_session: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            pending_splits: Mutex::new(VecDeque::default()),
            pending_split_pane_ids: Mutex::new(VecDeque::default()),
            pending_new_tabs: Mutex::new(VecDeque::default()),
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
        self.inner.cmd_queue.lock().clear();
    }

    pub(crate) fn transport_disconnected(&self) {
        *self.inner.state.lock() = State::Exit;
        *self.inner.connection_state.lock() = crate::tab::TmuxConnectionState::Disconnected;
        self.inner
            .in_flight_operation_id
            .store(0, Ordering::Release);
    }

    pub fn mark_reconnecting(&self) {
        *self.inner.connection_state.lock() = crate::tab::TmuxConnectionState::Reconnecting;
    }

    pub fn request_retry(&self) {
        self.inner.retry_requested.store(true, Ordering::Release);
    }

    pub fn take_retry_request(&self) -> bool {
        self.inner.retry_requested.swap(false, Ordering::AcqRel)
    }

    pub fn rename_tab(&self, tab_id: TabId, title: String) -> anyhow::Result<()> {
        let window_id = self
            .inner
            .gui_tabs
            .lock()
            .values()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.tmux_window_id)
            .ok_or_else(|| anyhow::anyhow!("no tmux window for tab {tab_id}"))?;
        self.inner
            .cmd_queue
            .lock()
            .push_back(Box::new(RenameWindow { window_id, title }));
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        Ok(())
    }

    pub fn close_tab(&self, tab_id: TabId) -> anyhow::Result<()> {
        let window_id = self
            .inner
            .gui_tabs
            .lock()
            .values()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.tmux_window_id)
            .ok_or_else(|| anyhow::anyhow!("no tmux window for tab {tab_id}"))?;
        self.inner
            .cmd_queue
            .lock()
            .push_back(Box::new(KillWindow { window_id }));
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        Ok(())
    }
}

#[async_trait(?Send)]
impl Domain for TmuxDomain {
    async fn spawn(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        _window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .ok_or_else(|| anyhow::anyhow!("failed to create tmux new-window completion"))?;
        self.inner.pending_new_tabs.lock().push_back(completion);
        self.inner.create_tmux_window();
        future.await
    }

    async fn split_pane(
        &self,
        _source: SplitSource,
        tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mut promise = promise::Promise::new();
        if let Some(future) = promise.get_future() {
            {
                let mut pending_splits = self.inner.pending_splits.lock();
                let _ = self.inner.split_tmux_pane(tab, pane_id, split_request)?;
                pending_splits.push_back(promise);
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
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("Spawn_pane not yet implemented for TmuxDomain");
    }

    fn domain_id(&self) -> DomainId {
        self.inner.domain_id
    }

    fn domain_name(&self) -> &str {
        "tmux"
    }

    async fn attach(&self, _window_id: Option<crate::WindowId>) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        false
    }

    fn detach(&self) -> anyhow::Result<()> {
        anyhow::bail!("detach not implemented for TmuxDomain");
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}

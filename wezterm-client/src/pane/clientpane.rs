use crate::domain::ClientInner;
use crate::pane::mousestate::MouseState;
use crate::pane::renderable::{hydrate_lines, RenderableInner, RenderableState};
use anyhow::bail;
use async_trait::async_trait;
use codec::*;
use config::configuration;
use config::keyassignment::ScrollbackEraseMode;
use mux::domain::DomainId;
use mux::pane::{
    alloc_pane_id, CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId,
    Pattern, SearchResult, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::TabId;
use mux::{Mux, MuxNotification};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::input::KeyEvent;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, Progress, StableRowIndex,
    TerminalConfiguration, TerminalSize,
};

pub struct ClientPane {
    client: Arc<ClientInner>,
    local_pane_id: PaneId,
    pub remote_pane_id: PaneId,
    pub remote_tab_id: TabId,
    pub renderable: Mutex<RenderableState>,
    configured_palette: Mutex<ColorPalette>,
    palette: Mutex<ColorPalette>,
    application_palette: Mutex<bool>,
    initial_palette_sync_pending: Mutex<bool>,
    writer: Mutex<PaneWriter>,
    mouse: Arc<Mutex<MouseState>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    mouse_grabbed: Mutex<bool>,
    ignore_next_kill: Mutex<bool>,
    user_vars: Mutex<HashMap<String, String>>,
    config: Mutex<Option<Arc<dyn TerminalConfiguration>>>,
    unseen_output: Mutex<bool>,
    progress: Mutex<Progress>,
    font_scale: Mutex<Option<f64>>,
    resize_queue: Arc<Mutex<ResizeQueue>>,
}

#[derive(Debug, Clone, Copy)]
struct PendingResize {
    size: TerminalSize,
    preserve_split: bool,
    generation: u64,
}

#[derive(Debug, Default)]
struct ResizeQueue {
    pending: Option<PendingResize>,
    deferred_pending: Option<PendingResize>,
    worker_running: bool,
    generation: u64,
    discard_before_generation: u64,
}

fn pane_font_trace_enabled() -> bool {
    std::env::var_os("WEZTERM_PANE_FONT_TRACE").is_some()
}

const RESIZE_COALESCE_DELAY: Duration = Duration::from_millis(75);
const RESIZE_COALESCE_MAX_DELAY: Duration = Duration::from_millis(250);

async fn resize_remote_panes(
    client: Arc<ClientInner>,
    remote_tab_id: TabId,
    panes: Vec<ResizePane>,
) -> anyhow::Result<()> {
    client
        .client
        .resize_panes(ResizePanes {
            containing_tab_id: remote_tab_id,
            panes,
        })
        .await?;
    Ok(())
}

async fn resize_remote_pane(
    client: Arc<ClientInner>,
    remote_tab_id: TabId,
    remote_pane_id: PaneId,
    size: TerminalSize,
    preserve_split: bool,
    resize_generation: u64,
) -> anyhow::Result<()> {
    resize_remote_panes(
        client,
        remote_tab_id,
        vec![ResizePane {
            pane_id: remote_pane_id,
            size,
            preserve_split,
            resize_generation,
        }],
    )
    .await
}

impl ClientPane {
    pub fn new(
        client: &Arc<ClientInner>,
        remote_tab_id: TabId,
        remote_pane_id: PaneId,
        size: TerminalSize,
        title: &str,
        font_scale: Option<f64>,
    ) -> Self {
        let local_pane_id = alloc_pane_id();
        let writer = PaneWriter {
            client: Arc::clone(client),
            remote_pane_id,
        };

        let mouse = Arc::new(Mutex::new(MouseState::new(
            remote_pane_id,
            client.client.clone(),
        )));

        let fetch_limiter =
            RateLimiter::new(|config| config.ratelimit_mux_line_prefetches_per_second);

        let render = RenderableState {
            inner: RefCell::new(RenderableInner::new(
                client,
                remote_pane_id,
                local_pane_id,
                RenderableDimensions {
                    cols: size.cols as _,
                    viewport_rows: size.rows as _,
                    scrollback_rows: size.rows as _,
                    physical_top: 0,
                    scrollback_top: 0,
                    dpi: size.dpi,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                    reverse_video: false,
                },
                title,
                fetch_limiter,
            )),
        };

        let config = configuration();
        let palette: ColorPalette = config.resolved_palette.clone().into();

        // Advise the server of our palette preference
        promise::spawn::spawn({
            let palette = palette.clone();
            let client = Arc::clone(client);
            async move {
                client
                    .client
                    .set_configured_palette_for_pane(SetPalette {
                        pane_id: remote_pane_id,
                        palette,
                    })
                    .await
            }
        })
        .detach();

        Self {
            client: Arc::clone(client),
            mouse,
            remote_pane_id,
            local_pane_id,
            remote_tab_id,
            application_palette: Mutex::new(false),
            renderable: Mutex::new(render),
            writer: Mutex::new(writer),
            configured_palette: Mutex::new(palette.clone()),
            palette: Mutex::new(palette),
            initial_palette_sync_pending: Mutex::new(true),
            clipboard: Mutex::new(None),
            mouse_grabbed: Mutex::new(false),
            ignore_next_kill: Mutex::new(false),
            unseen_output: Mutex::new(false),
            user_vars: Mutex::new(HashMap::new()),
            config: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
            font_scale: Mutex::new(font_scale),
            resize_queue: Arc::new(Mutex::new(ResizeQueue::default())),
        }
    }

    pub fn set_local_font_scale_from_mux(&self, font_scale: Option<f64>) {
        *self.font_scale.lock() = font_scale;
    }

    pub fn set_local_font_scale(&self, font_scale: Option<f64>) -> anyhow::Result<()> {
        *self.font_scale.lock() = mux::pane::normalize_font_scale(font_scale)?;
        Ok(())
    }

    pub async fn process_unilateral(&self, pdu: Pdu) -> anyhow::Result<()> {
        match pdu {
            Pdu::GetPaneRenderChangesResponse(mut delta) => {
                *self.mouse_grabbed.lock() = delta.mouse_grabbed;

                let bonus_lines = std::mem::take(&mut delta.bonus_lines);
                let client = { Arc::clone(&self.renderable.lock().inner.borrow().client) };
                let bonus_lines = hydrate_lines(client, delta.pane_id, bonus_lines).await;

                self.renderable
                    .lock()
                    .inner
                    .borrow_mut()
                    .apply_changes_to_surface(delta, bonus_lines);
            }
            Pdu::SetClipboard(SetClipboard {
                clipboard,
                selection,
                ..
            }) => match self.clipboard.lock().as_ref() {
                Some(clip) => {
                    log::debug!(
                        "Pdu::SetClipboard pane={} remote={} {:?} {:?}",
                        self.local_pane_id,
                        self.remote_pane_id,
                        selection,
                        clipboard
                    );
                    clip.set_contents(selection, clipboard)?;
                }
                None => {
                    log::error!("ClientPane: Ignoring SetClipboard request {:?}", clipboard);
                }
            },
            Pdu::SetPalette(SetPalette { palette, .. }) => {
                let configured_palette = self.configured_palette.lock().clone();
                let palette_is_configured = palette == configured_palette;
                let mut initial_palette_sync_pending = self.initial_palette_sync_pending.lock();

                if *initial_palette_sync_pending && !palette_is_configured {
                    log::debug!(
                        "ignoring stale initial remote palette for pane {}; waiting for configured palette sync",
                        self.local_pane_id
                    );
                    return Ok(());
                }

                *initial_palette_sync_pending = false;
                *self.application_palette.lock() = !palette_is_configured;

                *self.palette.lock() = palette;
                let mux = Mux::get();
                self.renderable.lock().inner.borrow_mut().make_all_stale();
                mux.notify(MuxNotification::Alert {
                    pane_id: self.local_pane_id,
                    alert: Alert::PaletteChanged,
                });
            }
            Pdu::NotifyAlert(NotifyAlert { alert, .. }) => {
                let mux = Mux::get();
                match &alert {
                    Alert::SetUserVar { name, value } => {
                        self.user_vars.lock().insert(name.clone(), value.clone());
                    }
                    Alert::OutputSinceFocusLost => {
                        *self.unseen_output.lock() = true;
                        mux.notify(MuxNotification::Alert {
                            pane_id: self.local_pane_id,
                            alert: Alert::OutputSinceFocusLost,
                        });
                    }
                    Alert::Progress(progress) => {
                        *self.progress.lock() = progress.clone();
                        mux.notify(MuxNotification::Alert {
                            pane_id: self.local_pane_id,
                            alert: Alert::Progress(progress.clone()),
                        });
                    }
                    _ => {}
                }
                mux.notify(MuxNotification::Alert {
                    pane_id: self.local_pane_id,
                    alert,
                });
            }
            Pdu::PaneRemoved(PaneRemoved { pane_id }) => {
                log::trace!("remote pane {} has been removed", pane_id);
                self.renderable.lock().inner.borrow_mut().dead = true;
                let mux = Mux::get();
                mux.prune_dead_windows_preserving_split();

                self.client.expire_stale_mappings();
            }
            Pdu::PaneFocused(PaneFocused { pane_id }) => {
                // We get here whenever the pane focus is changed on the
                // server. That might be due to the user here in the GUI
                // doing things, or it may be due to a "remote"
                // `wezterm cli activate-pane-direction` or similar call
                // from some other actor.
                // The latter case is the important one: it is desirable
                // for the focus change to be reflected locally after it
                // has been changed on the server, so we work to apply
                // it here.
                log::trace!("advised of remote pane focus: {pane_id}");

                let mux = Mux::get();
                if let Err(err) = mux.focus_pane_and_containing_tab(self.local_pane_id) {
                    log::error!("Error reconciling remote PaneFocused notification: {err:#}");
                }
            }
            _ => bail!("unhandled unilateral pdu: {:?}", pdu),
        };
        Ok(())
    }

    pub fn remote_pane_id(&self) -> TabId {
        self.remote_pane_id
    }

    pub fn resize_preserving_split(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.resize_impl(size, true, true)
    }

    fn enqueue_resize(&self, size: TerminalSize, preserve_split: bool, defer_remote: bool) {
        {
            let mut queue = self.resize_queue.lock();
            queue.generation = queue.generation.saturating_add(1);
            let pending = PendingResize {
                size,
                preserve_split,
                generation: queue.generation,
            };
            if defer_remote {
                queue.deferred_pending = Some(pending);
            } else {
                queue.pending = Some(pending);
            }
            if pane_font_trace_enabled() {
                log::info!(
                    target: "pane_font_trace",
                    "client enqueue local={} remote={} tab={} generation={} preserve_split={} defer_remote={} size={}x{} px={}x{} dpi={} worker_running={}",
                    self.local_pane_id,
                    self.remote_pane_id,
                    self.remote_tab_id,
                    pending.generation,
                    preserve_split,
                    defer_remote,
                    size.cols,
                    size.rows,
                    size.pixel_width,
                    size.pixel_height,
                    size.dpi,
                    queue.worker_running,
                );
            }
            if queue.worker_running {
                return;
            }
            if defer_remote {
                return;
            }
            queue.worker_running = true;
        }

        let queue = Arc::clone(&self.resize_queue);
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id;

        promise::spawn::spawn(async move {
            loop {
                let pending = {
                    let mut queue = queue.lock();
                    queue.pending.take()
                };

                if let Some(mut pending) = pending {
                    let coalesce_started = Instant::now();
                    smol::Timer::after(RESIZE_COALESCE_DELAY).await;
                    loop {
                        if coalesce_started.elapsed() >= RESIZE_COALESCE_MAX_DELAY {
                            break;
                        }
                        if let Some(latest) = {
                            let mut queue = queue.lock();
                            queue.pending.take()
                        } {
                            pending = latest;
                            smol::Timer::after(RESIZE_COALESCE_DELAY).await;
                        } else {
                            break;
                        }
                    }

                    let discard_before_generation = {
                        let queue = queue.lock();
                        queue.discard_before_generation
                    };
                    if pending.generation < discard_before_generation {
                        if pane_font_trace_enabled() {
                            log::info!(
                                target: "pane_font_trace",
                                "client discard-stale remote={} tab={} generation={} discard_before={} size={}x{} px={}x{}",
                                remote_pane_id,
                                remote_tab_id,
                                pending.generation,
                                discard_before_generation,
                                pending.size.cols,
                                pending.size.rows,
                                pending.size.pixel_width,
                                pending.size.pixel_height,
                            );
                        }
                        continue;
                    }

                    if pane_font_trace_enabled() {
                        log::info!(
                            target: "pane_font_trace",
                            "client send remote={} tab={} generation={} preserve_split={} size={}x{} px={}x{} dpi={}",
                            remote_pane_id,
                            remote_tab_id,
                            pending.generation,
                            pending.preserve_split,
                            pending.size.cols,
                            pending.size.rows,
                            pending.size.pixel_width,
                            pending.size.pixel_height,
                            pending.size.dpi,
                        );
                    }

                    if let Err(err) = resize_remote_pane(
                        Arc::clone(&client),
                        remote_tab_id,
                        remote_pane_id,
                        pending.size,
                        pending.preserve_split,
                        pending.generation,
                    )
                    .await
                    {
                        log::error!(
                            "failed to resize remote pane {} to {:?}: {:#}",
                            remote_pane_id,
                            pending.size,
                            err
                        );
                    }
                    continue;
                }

                let should_stop = {
                    let mut queue = queue.lock();
                    if queue.pending.is_none() {
                        queue.worker_running = false;
                        true
                    } else {
                        false
                    }
                };

                if should_stop {
                    break;
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn flush_pending_resize(&self, preserve_split: bool) {
        let size = self.current_size();

        let resize_generation = self.mark_resize_queue_flushed(size, preserve_split);

        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id;
        promise::spawn::spawn(async move {
            if let Err(err) = resize_remote_pane(
                client,
                remote_tab_id,
                remote_pane_id,
                size,
                preserve_split,
                resize_generation,
            )
            .await
            {
                log::error!(
                    "failed to flush resize for remote pane {} to {:?}: {:#}",
                    remote_pane_id,
                    size,
                    err
                );
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn has_pending_resize(&self) -> bool {
        let queue = self.resize_queue.lock();
        queue.pending.is_some() || queue.deferred_pending.is_some()
    }

    pub async fn flush_pending_resize_now(&self, preserve_split: bool) -> anyhow::Result<()> {
        let size = self.current_size();
        let resize_generation = self.mark_resize_queue_flushed(size, preserve_split);

        resize_remote_pane(
            Arc::clone(&self.client),
            self.remote_tab_id,
            self.remote_pane_id,
            size,
            preserve_split,
            resize_generation,
        )
        .await?;

        Ok(())
    }

    pub fn take_pending_resize_for_tab_sync(&self, preserve_split: bool) -> Option<ResizePane> {
        if !self.has_pending_resize() {
            return None;
        }

        let size = self.current_size();
        let resize_generation = self.mark_resize_queue_flushed(size, preserve_split);
        Some(ResizePane {
            pane_id: self.remote_pane_id,
            size,
            preserve_split,
            resize_generation,
        })
    }

    pub async fn flush_pending_tab_resize_now(&self, panes: Vec<ResizePane>) -> anyhow::Result<()> {
        if panes.is_empty() {
            return Ok(());
        }

        if pane_font_trace_enabled() {
            let summary = panes
                .iter()
                .map(|pane| {
                    format!(
                        "{}:{}x{}+{}x{}#{}:{}",
                        pane.pane_id,
                        pane.size.cols,
                        pane.size.rows,
                        pane.size.pixel_width,
                        pane.size.pixel_height,
                        pane.resize_generation,
                        pane.preserve_split
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            log::info!(
                target: "pane_font_trace",
                "client flush-batch tab={} panes={} [{}]",
                self.remote_tab_id,
                panes.len(),
                summary,
            );
        }

        let result = resize_remote_panes(Arc::clone(&self.client), self.remote_tab_id, panes).await;
        if pane_font_trace_enabled() {
            match &result {
                Ok(()) => log::info!(
                    target: "pane_font_trace",
                    "client flush-batch sent tab={}",
                    self.remote_tab_id
                ),
                Err(err) => log::error!(
                    target: "pane_font_trace",
                    "client flush-batch send failed tab={}: {err:#}",
                    self.remote_tab_id
                ),
            }
        }
        result
    }

    pub async fn flush_pending_split_resize_now(
        &self,
        split_index: usize,
        delta: isize,
        panes: Vec<ResizePane>,
    ) -> anyhow::Result<()> {
        self.client
            .client
            .resize_split(ResizeSplit {
                containing_tab_id: self.remote_tab_id,
                split_index,
                delta,
                panes,
            })
            .await?;
        Ok(())
    }

    fn current_size(&self) -> TerminalSize {
        let size = {
            let render = self.renderable.lock();
            let inner = render.inner.borrow();
            TerminalSize {
                rows: inner.dimensions.viewport_rows,
                cols: inner.dimensions.cols,
                pixel_width: inner.dimensions.pixel_width,
                pixel_height: inner.dimensions.pixel_height,
                dpi: inner.dimensions.dpi,
            }
        };

        size
    }

    fn mark_resize_queue_flushed(&self, size: TerminalSize, preserve_split: bool) -> u64 {
        {
            let mut queue = self.resize_queue.lock();
            queue.generation = queue.generation.saturating_add(1);
            queue.discard_before_generation = queue.generation;
            queue.pending.take();
            queue.deferred_pending.take();
            if pane_font_trace_enabled() {
                log::info!(
                    target: "pane_font_trace",
                    "client flush local={} remote={} tab={} generation={} preserve_split={} size={}x{} px={}x{} dpi={}",
                    self.local_pane_id,
                    self.remote_pane_id,
                    self.remote_tab_id,
                    queue.generation,
                    preserve_split,
                    size.cols,
                    size.rows,
                    size.pixel_width,
                    size.pixel_height,
                    size.dpi,
                );
            }
            queue.generation
        }
    }

    pub fn connection_status_label(&self) -> Option<String> {
        let render = self.renderable.lock();
        let inner = render.inner.borrow();

        if !inner.client.client.is_connected() {
            Some("disconnected".to_string())
        } else if inner.is_tardy() {
            Some(format!("stalled {:.0?}", inner.last_recv_time.elapsed()))
        } else {
            None
        }
    }

    fn resize_impl(
        &self,
        size: TerminalSize,
        preserve_split: bool,
        defer_remote: bool,
    ) -> anyhow::Result<()> {
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();

        let cols = size.cols as usize;
        let rows = size.rows as usize;

        let dimensions_changed = inner.dimensions.cols != cols
            || inner.dimensions.viewport_rows != rows
            || inner.dimensions.pixel_width != size.pixel_width
            || inner.dimensions.pixel_height != size.pixel_height
            || inner.dimensions.dpi != size.dpi;

        if pane_font_trace_enabled() {
            log::info!(
                target: "pane_font_trace",
                "client resize-impl local={} remote={} tab={} preserve_split={} changed={} actual={}x{} px={}x{} next={}x{} px={}x{} dpi={}",
                self.local_pane_id,
                self.remote_pane_id,
                self.remote_tab_id,
                preserve_split,
                dimensions_changed,
                inner.dimensions.cols,
                inner.dimensions.viewport_rows,
                inner.dimensions.pixel_width,
                inner.dimensions.pixel_height,
                size.cols,
                size.rows,
                size.pixel_width,
                size.pixel_height,
                size.dpi,
            );
        }

        if dimensions_changed {
            inner.dimensions.cols = cols;
            inner.dimensions.viewport_rows = rows;
            inner.dimensions.pixel_width = size.pixel_width;
            inner.dimensions.pixel_height = size.pixel_height;
            inner.dimensions.dpi = size.dpi;

            // Invalidate any cached rows on a resize
            inner.make_all_stale();
        }

        if dimensions_changed {
            self.enqueue_resize(size, preserve_split, defer_remote);
        }
        if dimensions_changed {
            inner.update_last_send();
        }
        Ok(())
    }

    /// Arrange to suppress the next Pane::kill call.
    /// This is a bit of a hack that we use when closing a window;
    /// our Domain::local_window_is_closing impl calls this for each
    /// ClientPane in the window so that closing a window effectively
    /// "detaches" the window so that reconnecting later will resume
    /// from where they left off.
    /// It isn't perfect.
    pub fn ignore_next_kill(&self) {
        *self.ignore_next_kill.lock() = true;
    }
}

#[async_trait(?Send)]
impl Pane for ClientPane {
    fn pane_id(&self) -> PaneId {
        self.local_pane_id
    }

    fn get_metadata(&self) -> Value {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();

        let mut map: BTreeMap<Value, Value> = BTreeMap::new();
        map.insert(
            Value::String("is_tardy".to_string()),
            Value::Bool(inner.is_tardy()),
        );
        map.insert(
            Value::String("since_last_response_ms".to_string()),
            Value::U64(inner.last_recv_time.elapsed().as_millis() as u64),
        );

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        self.renderable.lock().get_cursor_position()
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.renderable.lock().get_dimensions()
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        self.renderable.lock().get_lines(lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.renderable.lock().get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        self.renderable.lock().get_changed_since(lines, seqno)
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.clipboard.lock().replace(Arc::clone(clipboard));
    }

    fn get_title(&self) -> String {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();
        inner.title.clone()
    }

    fn get_progress(&self) -> Progress {
        self.progress.lock().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .predict_from_paste(text);

        let data = text.to_owned();
        promise::spawn::spawn(async move {
            client
                .client
                .send_paste(SendPaste {
                    pane_id: remote_pane_id,
                    data,
                })
                .await
        })
        .detach();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn set_zoomed(&self, zoomed: bool) {
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id;
        // Invalidate any cached rows on a resize
        inner.make_all_stale();
        promise::spawn::spawn(async move {
            client
                .client
                .set_zoomed(SetPaneZoomed {
                    containing_tab_id: remote_tab_id,
                    pane_id: remote_pane_id,
                    zoomed,
                })
                .await
        })
        .detach();
        inner.update_last_send();
    }

    fn set_left_sidebar_hidden(&self, hidden: bool) {
        let client = Arc::clone(&self.client);
        let remote_tab_id = self.remote_tab_id;
        promise::spawn::spawn(async move {
            client
                .client
                .set_left_sidebar_hidden(SetLeftSidebarHidden {
                    containing_tab_id: remote_tab_id,
                    hidden,
                })
                .await
        })
        .detach();
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.resize_impl(size, false, true)
    }

    fn resize_preserving_split(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.resize_impl(size, true, true)
    }

    fn resize_preserving_split_for_split_drag(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.resize_impl(size, true, true)
    }

    fn font_scale(&self) -> Option<f64> {
        *self.font_scale.lock()
    }

    fn set_font_scale(&self, font_scale: Option<f64>) -> anyhow::Result<()> {
        let font_scale = mux::pane::normalize_font_scale(font_scale)?;
        *self.font_scale.lock() = font_scale;
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        promise::spawn::spawn(async move {
            if let Err(err) = client
                .client
                .set_pane_font_scale(SetPaneFontScale {
                    pane_id: remote_pane_id,
                    font_scale,
                })
                .await
            {
                log::error!(
                    "failed to persist pane {} font scale {:?}: {:#}",
                    remote_pane_id,
                    font_scale,
                    err
                );
            }
        })
        .detach();
        Ok(())
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        match self
            .client
            .client
            .search_scrollback(SearchScrollbackRequest {
                pane_id: self.remote_pane_id,
                pattern,
                range,
                limit,
            })
            .await
        {
            Ok(SearchScrollbackResponse { results }) => Ok(results),
            Err(e) => Err(e),
        }
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let input_serial;
        {
            let renderable = self.renderable.lock();
            let mut inner = renderable.inner.borrow_mut();
            inner.input_serial = InputSerial::now();
            input_serial = inner.input_serial;
            inner.predict_from_key_event(key, mods);
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        promise::spawn::spawn(async move {
            client
                .client
                .key_down(SendKeyDown {
                    pane_id: remote_pane_id,
                    event: KeyEvent {
                        key,
                        modifiers: mods,
                    },
                    input_serial,
                })
                .await
        })
        .detach();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        // TODO: decide how to handle key_up for mux client
        Ok(())
    }

    fn kill(&self) {
        let mut ignore = self.ignore_next_kill.lock();
        if *ignore {
            *ignore = false;
            return;
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let local_domain_id = self.client.local_domain_id;

        // We only want to ask the server to kill the pane if the user
        // explicitly requested it to die.
        // Domain detaching can implicitly call Pane::kill on the panes
        // in the domain, so we need to check here whether the domain is
        // in the detached state; if so then we must skip sending the
        // kill to the server.
        let mut send_kill = true;

        {
            let mux = Mux::get();
            if let Some(client_domain) = mux.get_domain(local_domain_id) {
                if client_domain.state() == mux::domain::DomainState::Detached {
                    send_kill = false;
                }
            }
        }

        if send_kill {
            promise::spawn::spawn(async move {
                client
                    .client
                    .kill_pane(KillPane {
                        pane_id: remote_pane_id,
                    })
                    .await
            })
            .detach();
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        self.mouse.lock().append(event);
        if MouseState::next(Arc::clone(&self.mouse)) {
            self.renderable.lock().inner.borrow_mut().update_last_send();
        }
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.renderable.lock().inner.borrow().dead
    }

    fn palette(&self) -> ColorPalette {
        self.palette.lock().clone()
    }

    fn domain_id(&self) -> DomainId {
        self.client.local_domain_id
    }

    fn is_mouse_grabbed(&self) -> bool {
        *self.mouse_grabbed.lock()
    }

    fn is_alt_screen_active(&self) -> bool {
        // FIXME: retrieve this from the remote
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.renderable.lock().inner.borrow().working_dir.clone()
    }

    fn focus_changed(&self, focused: bool) {
        if focused {
            self.advise_focus();
            *self.unseen_output.lock() = false;
        }
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        promise::spawn::spawn(async move {
            client
                .client
                .erase_scrollback(EraseScrollbackRequest {
                    pane_id: remote_pane_id,
                    erase_mode,
                })
                .await
        })
        .detach();
    }

    fn advise_focus(&self) {
        let mut focused_pane = self.client.focused_remote_pane_id.lock().unwrap();
        if *focused_pane != Some(self.remote_pane_id) {
            focused_pane.replace(self.remote_pane_id);
            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            promise::spawn::spawn(async move {
                client
                    .client
                    .set_focused_pane_id(SetFocusedPane {
                        pane_id: remote_pane_id,
                    })
                    .await
            })
            .detach();
        }
    }

    fn has_unseen_output(&self) -> bool {
        *self.unseen_output.lock()
    }

    fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        match reason {
            CloseReason::Window => true,
            CloseReason::Tab => false,
            CloseReason::Pane => false,
        }
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.user_vars.lock().clone()
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        let palette = config.color_palette();
        // If the application running in the pane hasn't changed the
        // palette through escape sequences, speculatively adopt the
        // new palette so that it updates with the lowest latency.
        if !*self.application_palette.lock() {
            *self.palette.lock() = palette.clone();
            self.renderable.lock().inner.borrow_mut().make_all_stale();
            Mux::get().notify(MuxNotification::Alert {
                pane_id: self.local_pane_id,
                alert: Alert::PaletteChanged,
            });
        }
        *self.configured_palette.lock() = palette.clone();

        // and now send the color palette to the server
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        promise::spawn::spawn(async move {
            client
                .client
                .set_configured_palette_for_pane(SetPalette {
                    pane_id: remote_pane_id,
                    palette,
                })
                .await
        })
        .detach();
        self.config.lock().replace(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        self.config.lock().clone()
    }
}

struct PaneWriter {
    client: Arc<ClientInner>,
    remote_pane_id: TabId,
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        promise::spawn::block_on(self.client.client.write_to_pane(WriteToPane {
            pane_id: self.remote_pane_id,
            data: data.to_vec(),
        }))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

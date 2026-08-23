use crate::resize_increment_calculator::ResizeIncrementCalculator;
use crate::utilsprites::RenderMetrics;
use ::window::{Dimensions, ResizeIncrement, Window, WindowOps, WindowState};
use codec::ResizePane;
use config::{ConfigHandle, DimensionContext};
use mux::pane::{normalize_font_scale, Pane, PaneId};
use mux::tab::{PositionedPane, Tab};
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use ordered_float::NotNan;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use wezterm_font::FontConfiguration;
use wezterm_term::TerminalSize;

static CONNECTED_TAB_RESIZE_FLUSHES: LazyLock<Mutex<HashSet<MuxWindowId>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static CONNECTED_SPLIT_DRAGS: LazyLock<Mutex<HashSet<MuxWindowId>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, Copy)]
pub struct RowsAndCols {
    pub rows: usize,
    pub cols: usize,
}

#[derive(Debug)]
pub enum ScaleChange {
    Absolute(f64),
    Relative(f64),
}

fn pane_font_trace_enabled() -> bool {
    std::env::var_os("WEZTERM_PANE_FONT_TRACE").is_some()
}

/// A connected mux tab can contain panes with different font scales, so its
/// aggregate row/column count is not comparable to the GUI window's base-font
/// grid. Its physical extent is authoritative during remote reconciliation.
fn connected_tab_needs_physical_resize(tab: TerminalSize, window: TerminalSize) -> bool {
    if tab.dpi != window.dpi {
        return true;
    }

    // Nested split calculations can accumulate a handful of pixels of
    // rounding error. Resizing the complete tab to correct less than one base
    // cell destroys the font-aware row/column counts and feeds the result back
    // to the mux. A real terminal-grid resize crosses at least one base cell.
    let cell_width = window
        .pixel_width
        .checked_div(window.cols)
        .unwrap_or(1)
        .max(1);
    let cell_height = window
        .pixel_height
        .checked_div(window.rows)
        .unwrap_or(1)
        .max(1);

    tab.pixel_width.abs_diff(window.pixel_width) >= cell_width
        || tab.pixel_height.abs_diff(window.pixel_height) >= cell_height
}

fn connected_tab_resize_has_pending(mux_window_id: MuxWindowId) -> bool {
    let Some(tab) = Mux::get().get_active_tab_for_window(mux_window_id) else {
        return false;
    };

    tab.iter_panes_ignoring_zoom().into_iter().any(|pos| {
        pos.pane
            .downcast_ref::<wezterm_client::pane::ClientPane>()
            .is_some_and(|client_pane| client_pane.has_pending_resize())
    })
}

fn connected_split_drag_active(mux_window_id: MuxWindowId) -> bool {
    CONNECTED_SPLIT_DRAGS
        .lock()
        .expect("CONNECTED_SPLIT_DRAGS mutex poisoned")
        .contains(&mux_window_id)
}

fn take_connected_tab_resize_batch(
    mux_window_id: MuxWindowId,
    preserve_split: bool,
) -> Option<(Arc<dyn Pane>, Vec<ResizePane>)> {
    let tab = Mux::get().get_active_tab_for_window(mux_window_id)?;

    let mut batch = vec![];
    let mut batch_sender = None;

    for pane in tab
        .iter_panes_ignoring_zoom()
        .into_iter()
        .filter_map(|pos| {
            let pane = Arc::clone(&pos.pane);
            if let Some(client_pane) = pane.downcast_ref::<wezterm_client::pane::ClientPane>() {
                client_pane.has_pending_resize().then_some(pane)
            } else {
                None
            }
        })
    {
        if let Some(client_pane) = pane.downcast_ref::<wezterm_client::pane::ClientPane>() {
            if batch_sender.is_none() {
                batch_sender = Some(Arc::clone(&pane));
            }
            if let Some(resize) = client_pane.take_pending_resize_for_tab_sync(preserve_split) {
                batch.push(resize);
            }
        }
    }

    batch_sender.map(|sender| (sender, batch))
}

impl super::TermWindow {
    pub fn resize(
        &mut self,
        dimensions: Dimensions,
        window_state: WindowState,
        window: &Window,
        live_resizing: bool,
    ) {
        log::trace!(
            "resize event, live={} current cells: {:?}, current dims: {:?}, new dims: {:?} window_state:{:?}",
            live_resizing,
            self.current_cell_dimensions(),
            self.dimensions,
            dimensions,
            window_state,
        );
        if dimensions.pixel_width == 0 || dimensions.pixel_height == 0 {
            // on windows, this can happen when minimizing the window.
            // NOP!
            log::trace!("new dimensions are zero: NOP!");
            return;
        }
        if self.dimensions == dimensions && self.window_state == window_state {
            // It didn't really change
            log::trace!("dimensions didn't change NOP!");
            return;
        }
        let last_state = self.window_state;
        self.window_state = window_state;
        self.quad_generation += 1;
        if last_state != self.window_state {
            self.load_os_parameters();
        }

        if let Some(webgpu) = self.webgpu.as_mut() {
            webgpu.resize(dimensions);
        }

        // For simple, user-interactive resizes where the dpi doesn't change,
        // skip our scaling recalculation
        if live_resizing && self.dimensions.dpi == dimensions.dpi {
            self.apply_dimensions(&dimensions, None, window);
        } else {
            self.scaling_changed(dimensions, self.fonts.get_font_scale(), window);
        }
        if let Some(modal) = self.get_modal() {
            modal.reconfigure(self);
        }
        self.emit_window_event("window-resized", None);
    }

    pub fn apply_pending_scale_changes(&mut self) {
        while self.resizes_pending == 0 {
            match self.pending_scale_changes.pop_front() {
                Some(ScaleChange::Relative(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(self.fonts.get_font_scale() * change, &window);
                    }
                }
                Some(ScaleChange::Absolute(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(change, &window);
                    }
                }
                None => break,
            }
        }
    }

    pub fn apply_scale_change(&mut self, dimensions: &Dimensions, font_scale: f64) {
        let config = &self.config;
        let font_size = config.font_size * font_scale;
        let theoretical_height = font_size * dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small font scale {:?}
                       font_scale={} would yield font_height {}",
                dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let (prior_font, prior_dpi) = self.fonts.change_scaling(font_scale, dimensions.dpi);
        match RenderMetrics::new(&self.fonts) {
            Ok(metrics) => {
                self.render_metrics = metrics;
                self.clear_pane_font_metrics_cache();
            }
            Err(err) => {
                log::error!(
                    "{:#} while attempting to scale font to {} with {:?}",
                    err,
                    font_scale,
                    dimensions
                );
                // Restore prior scaling factors
                self.fonts.change_scaling(prior_font, prior_dpi);
            }
        }

        if let Err(err) = self.recreate_texture_atlas(None) {
            log::error!("recreate_texture_atlas: {:#}", err);
        }
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
    }

    pub fn apply_dimensions(
        &mut self,
        dimensions: &Dimensions,
        mut scale_changed_cells: Option<RowsAndCols>,
        window: &Window,
    ) {
        log::trace!(
            "apply_dimensions {:?} scale_changed_cells {:?}. window_state {:?}",
            dimensions,
            scale_changed_cells,
            self.window_state
        );
        let saved_dims = self.dimensions;
        self.dimensions = *dimensions;
        self.quad_generation += 1;

        if scale_changed_cells.is_some() && !self.window_state.can_resize() {
            log::warn!(
                "cannot resize window to match {:?} because window_state is {:?}",
                scale_changed_cells,
                self.window_state
            );
            scale_changed_cells.take();
        }

        // Technically speaking, we should compute the rows and cols
        // from the new dimensions and apply those to the tabs, and
        // then for the scaling changed case, try to re-apply the
        // original rows and cols, but if we do that we end up
        // double resizing the tabs, so we speculatively apply the
        // final size, which in that case should result in a NOP
        // change to the tab size.

        let config = &self.config;

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };

        let border = self.get_os_border();

        let (size, dims, ri_calc) = if let Some(cell_dims) = scale_changed_cells {
            // Scaling preserves existing terminal dimensions, yielding a new
            // overall set of window dimensions
            let size = TerminalSize {
                rows: cell_dims.rows,
                cols: cell_dims.cols,
                pixel_height: cell_dims.rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cell_dims.cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let rows = size.rows;
            let cols = size.cols;

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right = effective_right_padding(&config, h_context);

            let pixel_height = (rows * self.render_metrics.cell_size.height as usize)
                + (padding_top + padding_bottom)
                + (border.top + border.bottom).get() as usize
                + tab_bar_height as usize;

            let pixel_width = (cols * self.render_metrics.cell_size.width as usize)
                + (padding_left + padding_right)
                + (border.left + border.right).get() as usize;

            let dims = Dimensions {
                pixel_width: pixel_width as usize,
                pixel_height: pixel_height as usize,
                dpi: dimensions.dpi,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left: padding_left,
                padding_top: padding_top,
                padding_right: padding_right,
                padding_bottom: padding_bottom,
                border: border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, dims, ri_calc)
        } else {
            // Resize of the window dimensions may result in changed terminal dimensions

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right = effective_right_padding(&config, h_context);

            let avail_width = dimensions.pixel_width.saturating_sub(
                (padding_left + padding_right) as usize
                    + (border.left + border.right).get() as usize,
            );
            let avail_height = dimensions
                .pixel_height
                .saturating_sub(
                    (padding_top + padding_bottom) as usize
                        + (border.top + border.bottom).get() as usize,
                )
                .saturating_sub(tab_bar_height as usize);

            let rows = avail_height / self.render_metrics.cell_size.height as usize;
            let cols = avail_width / self.render_metrics.cell_size.width as usize;

            let size = TerminalSize {
                rows,
                cols,
                // Take care to use the exact pixel dimensions of the cells, rather
                // than the available space, so that apps that are sensitive to
                // the pixels-per-cell have consistent values at a given font size.
                // https://github.com/wezterm/wezterm/issues/535
                pixel_height: rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left: padding_left,
                padding_top: padding_top,
                padding_right: padding_right,
                padding_bottom: padding_bottom,
                border: border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, *dimensions, ri_calc)
        };

        log::trace!("apply_dimensions computed size {:?}, dims {:?}", size, dims);

        self.terminal_size = size;

        let mux = Mux::get();
        if let Some(window) = mux.get_window(self.mux_window_id) {
            for tab in window.iter() {
                tab.resize_preserving_split(size);
                self.resize_tab_panes_for_font_scale(&tab);
            }
        };
        self.flush_connected_tab_resize(true);
        self.resize_overlays();
        self.invalidate_fancy_tab_bar();
        self.update_title();

        window.set_resize_increments(if self.config.use_resize_increments {
            ri_calc.into()
        } else {
            ResizeIncrement::disabled()
        });

        // Queue up a speculative resize in order to preserve the number of rows+cols
        if let Some(cell_dims) = scale_changed_cells {
            // If we don't think the dimensions have changed, don't request
            // the window to change.  This seems to help on Wayland where
            // we won't know what size the compositor thinks we should have
            // when we're first opened, until after it sends us a configure event.
            // If we send this too early, it will trump that configure event
            // and we'll end up with weirdness where our window renders in the
            // middle of a larger region that the compositor thinks we live in.
            // Wayland is weird!
            if saved_dims != dims {
                log::trace!(
                    "scale changed so resize from {:?} to {:?} {:?} (event called with {:?})",
                    saved_dims,
                    dims,
                    cell_dims,
                    dimensions
                );
                // Stash this size pre-emptively. Without this, on Windows,
                // when the font scaling is changed we can end up not seeing
                // these dimensions and the scaling_changed logic ends up
                // comparing two dimensions that have the same DPI and recomputing
                // an adjusted terminal size.
                // eg: rather than a simple old-dpi -> new dpi transition, we'd
                // see old-dpi -> new dpi, call set_inner_size, then see a
                // new-dpi -> new-dpi adjustment with a slightly different
                // pixel geometry which is considered to be a user-driven resize.
                // Stashing the dimensions here avoids that misconception.
                self.dimensions = dims;
                self.set_inner_size(window, dims.pixel_width, dims.pixel_height);
            }
        }
    }

    pub fn current_cell_dimensions(&self) -> RowsAndCols {
        RowsAndCols {
            rows: self.terminal_size.rows as usize,
            cols: self.terminal_size.cols as usize,
        }
    }

    #[allow(clippy::float_cmp)]
    pub fn scaling_changed(&mut self, dimensions: Dimensions, font_scale: f64, window: &Window) {
        fn dpi_adjusted(n: usize, dpi: usize) -> f32 {
            n as f32 / dpi as f32
        }

        /// On Windows, scaling changes may adjust the pixel geometry by a few pixels,
        /// so this function checks if we're in a close-enough ballpark.
        fn close_enough(a: f32, b: f32) -> bool {
            let diff = (a - b).abs();
            diff < 10.
        }

        // Distinguish between eg: dpi being detected as double the initial dpi (where
        // the pixel dimensions don't change), and the dpi change being detected, but
        // where the window manager also decides to tile/resize the window.
        // In the latter case, we don't want to preserve the terminal rows/cols.
        let simple_dpi_change = dimensions.dpi != self.dimensions.dpi
            && ((close_enough(
                dpi_adjusted(dimensions.pixel_height, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_height, self.dimensions.dpi),
            ) && close_enough(
                dpi_adjusted(dimensions.pixel_width, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_width, self.dimensions.dpi),
            )) || (close_enough(
                dimensions.pixel_width as f32,
                self.dimensions.pixel_width as f32,
            ) && close_enough(
                dimensions.pixel_height as f32,
                self.dimensions.pixel_height as f32,
            )));

        if simple_dpi_change && cfg!(target_os = "macos") {
            // Spooky action at a distance: on macOS, NSWindow::isZoomed can falsely
            // return YES in situations such as the current screen changing.
            // That causes window_state to believe that we are MAXIMIZED.
            // We cannot easily detect that in the window layer, but at this
            // layer, if we realize that the dpi was the only thing that changed
            // then remove the MAXIMIZED state so that the can_resize check
            // in adjust_font_scale will not block us from adapting to the new
            // DPI. This is gross and it would be better handled at the macOS
            // layer.
            // <https://github.com/wezterm/wezterm/issues/3503>
            self.window_state -= WindowState::MAXIMIZED;
        }

        let dpi_changed = dimensions.dpi != self.dimensions.dpi;
        let font_scale_changed = font_scale != self.fonts.get_font_scale();
        let scale_changed = dpi_changed || font_scale_changed;

        log::trace!(
            "dpi_changed={}, font_scale_changed={} scale_changed={} simple_dpi_change={}",
            dpi_changed,
            font_scale_changed,
            scale_changed,
            simple_dpi_change
        );

        let cell_dims = self.current_cell_dimensions();

        if scale_changed {
            self.apply_scale_change(&dimensions, font_scale);
        }

        let scale_changed_cells = if font_scale_changed || simple_dpi_change {
            Some(cell_dims)
        } else {
            None
        };

        log::trace!(
            "scaling_changed, follow with applying dimensions. scale_changed_cells={:?}",
            scale_changed_cells
        );
        self.apply_dimensions(&dimensions, scale_changed_cells, window);
    }

    /// Used for applying font size changes only; this takes into account
    /// the `adjust_window_size_when_changing_font_size` configuration and
    /// revises the scaling/resize change accordingly
    pub fn adjust_font_scale(&mut self, font_scale: f64, window: &Window) {
        let adjust_window_size_when_changing_font_size =
            match self.config.adjust_window_size_when_changing_font_size {
                Some(value) => value,
                None => {
                    let is_tiling = self
                        .config
                        .tiling_desktop_environments
                        .iter()
                        .any(|item| item.as_str() == self.connection_name.as_str());
                    !is_tiling
                }
            };

        if self.window_state.can_resize() && adjust_window_size_when_changing_font_size {
            self.scaling_changed(self.dimensions, font_scale, window);
        } else {
            let dimensions = self.dimensions;
            // Compute new font metrics
            self.apply_scale_change(&dimensions, font_scale);
            // Now revise the pty size to fit the window
            self.apply_dimensions(&dimensions, None, window);
        }
    }

    pub fn decrease_font_size(&mut self) {
        self.pending_scale_changes
            .push_back(ScaleChange::Relative(1.0 / 1.1));
        self.apply_pending_scale_changes();
    }

    pub fn increase_font_size(&mut self) {
        self.pending_scale_changes
            .push_back(ScaleChange::Relative(1.1));
        self.apply_pending_scale_changes();
    }

    pub fn reset_font_size(&mut self) {
        self.pending_scale_changes
            .push_back(ScaleChange::Absolute(1.0));
        self.apply_pending_scale_changes();
    }

    pub fn pane_font_scale(&self, pane_id: PaneId) -> f64 {
        self.pane_state
            .borrow()
            .get(&pane_id)
            .and_then(|state| state.font_scale)
            .or_else(|| {
                Mux::get()
                    .get_pane(pane_id)
                    .and_then(|pane| normalize_font_scale(pane.font_scale()).ok().flatten())
            })
            .unwrap_or(1.0)
    }

    pub fn pane_font_configuration(
        &self,
        font_scale: f64,
    ) -> anyhow::Result<Rc<FontConfiguration>> {
        Ok(self.cached_pane_font_metrics(font_scale)?.fonts)
    }

    pub fn pane_render_metrics(&self, font_scale: f64) -> anyhow::Result<RenderMetrics> {
        Ok(self.cached_pane_font_metrics(font_scale)?.render_metrics)
    }

    pub fn clear_pane_font_metrics_cache(&self) {
        self.pane_font_metrics.borrow_mut().clear();
    }

    fn pane_font_cache_key(&self, font_scale: f64) -> anyhow::Result<NotNan<f64>> {
        NotNan::new(font_scale).map_err(|_| anyhow::anyhow!("invalid pane font scale {font_scale}"))
    }

    fn cached_pane_font_metrics(&self, font_scale: f64) -> anyhow::Result<super::PaneFontMetrics> {
        if font_scale == self.fonts.get_font_scale() {
            return Ok(super::PaneFontMetrics {
                fonts: Rc::clone(&self.fonts),
                render_metrics: self.render_metrics,
            });
        }

        let key = self.pane_font_cache_key(font_scale)?;
        if let Some(metrics) = self.pane_font_metrics.borrow().get(&key) {
            return Ok(metrics.clone());
        }

        let fonts = Rc::new(FontConfiguration::new(
            Some(self.config.clone()),
            self.dimensions.dpi,
        )?);
        fonts.change_scaling(font_scale, self.dimensions.dpi);
        let render_metrics = RenderMetrics::new(&fonts)?;

        let metrics = super::PaneFontMetrics {
            fonts,
            render_metrics,
        };
        self.pane_font_metrics
            .borrow_mut()
            .insert(key, metrics.clone());
        Ok(metrics)
    }

    fn pane_size_for_font_scale(
        &self,
        pixel_width: usize,
        pixel_height: usize,
        font_scale: f64,
    ) -> anyhow::Result<TerminalSize> {
        let metrics = self.pane_render_metrics(font_scale)?;
        let cell_width = (metrics.cell_size.width as usize).max(1);
        let cell_height = (metrics.cell_size.height as usize).max(1);
        let cols = (pixel_width / cell_width).max(1);
        let rows = (pixel_height / cell_height).max(1);

        Ok(TerminalSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
            dpi: self.dimensions.dpi as u32,
        })
    }

    fn resize_positioned_pane_for_font_scale(&self, pos: &PositionedPane) -> anyhow::Result<bool> {
        let font_scale = self.pane_font_scale(pos.pane.pane_id());
        let size = self.pane_size_for_font_scale(pos.pixel_width, pos.pixel_height, font_scale)?;
        let dims = pos.pane.get_dimensions();
        let is_client = pos
            .pane
            .downcast_ref::<wezterm_client::pane::ClientPane>()
            .is_some();

        if pane_font_trace_enabled() {
            log::info!(
                target: "pane_font_trace",
                "gui resize-check pane={} idx={} active={} zoomed={} client={} rect_cells={}x{}+{}+{} rect_px={}x{}+{}+{} font_scale={:.3} actual={}x{} px={}x{} dpi={} computed={}x{} px={}x{} dpi={}",
                pos.pane.pane_id(),
                pos.index,
                pos.is_active,
                pos.is_zoomed,
                is_client,
                pos.width,
                pos.height,
                pos.left,
                pos.top,
                pos.pixel_width,
                pos.pixel_height,
                pos.pixel_left,
                pos.pixel_top,
                font_scale,
                dims.cols,
                dims.viewport_rows,
                dims.pixel_width,
                dims.pixel_height,
                dims.dpi,
                size.cols,
                size.rows,
                size.pixel_width,
                size.pixel_height,
                size.dpi,
            );
        }

        if dims.cols == size.cols
            && dims.viewport_rows == size.rows
            && dims.pixel_width == size.pixel_width
            && dims.pixel_height == size.pixel_height
            && dims.dpi == size.dpi
        {
            return Ok(false);
        }

        if let Some(client_pane) = pos.pane.downcast_ref::<wezterm_client::pane::ClientPane>() {
            if pane_font_trace_enabled() {
                log::info!(
                    target: "pane_font_trace",
                    "gui resize-apply client pane={} size={}x{} px={}x{} preserve_split=true",
                    pos.pane.pane_id(),
                    size.cols,
                    size.rows,
                    size.pixel_width,
                    size.pixel_height,
                );
            }
            client_pane.resize_preserving_split(size)?;
        } else {
            if pane_font_trace_enabled() {
                log::info!(
                    target: "pane_font_trace",
                    "gui resize-apply local pane={} size={}x{} px={}x{}",
                    pos.pane.pane_id(),
                    size.cols,
                    size.rows,
                    size.pixel_width,
                    size.pixel_height,
                );
            }
            pos.pane.resize(size)?;
        }
        Ok(true)
    }

    fn resize_tab_panes_for_font_scale(&self, tab: &Arc<Tab>) {
        let visible = tab.iter_panes();
        let all = tab.iter_panes_ignoring_zoom();
        let zoomed_pane_id = visible
            .iter()
            .find(|pos| pos.is_zoomed)
            .map(|pos| pos.pane.pane_id());

        for pos in &visible {
            if let Err(err) = self.resize_positioned_pane_for_font_scale(pos) {
                log::error!(
                    "failed to resize visible pane {} for font scale: {:#}",
                    pos.pane.pane_id(),
                    err
                );
            }
        }

        for pos in &all {
            if Some(pos.pane.pane_id()) == zoomed_pane_id {
                continue;
            }
            if visible
                .iter()
                .any(|visible| visible.pane.pane_id() == pos.pane.pane_id())
            {
                continue;
            }
            if let Err(err) = self.resize_positioned_pane_for_font_scale(pos) {
                log::error!(
                    "failed to resize hidden pane {} for font scale: {:#}",
                    pos.pane.pane_id(),
                    err
                );
            }
        }
    }

    fn resize_active_tab_panes_for_font_scale(&self) {
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        self.resize_tab_panes_for_font_scale(&tab);
    }

    pub fn resize_tab_id_panes_for_font_scale(&self, tab_id: mux::tab::TabId) {
        let Some(tab) = Mux::get().get_tab(tab_id) else {
            return;
        };
        let tab_size = tab.get_size();
        let needs_physical_resize =
            connected_tab_needs_physical_resize(tab_size, self.terminal_size);
        if needs_physical_resize && pane_font_trace_enabled() {
            log::info!(
                target: "pane_font_trace",
                "gui connected-tab physical-resize tab={} actual={}x{} px={}x{} dpi={} window={}x{} px={}x{} dpi={}",
                tab_id,
                tab_size.cols,
                tab_size.rows,
                tab_size.pixel_width,
                tab_size.pixel_height,
                tab_size.dpi,
                self.terminal_size.cols,
                self.terminal_size.rows,
                self.terminal_size.pixel_width,
                self.terminal_size.pixel_height,
                self.terminal_size.dpi,
            );
        }
        if needs_physical_resize {
            tab.resize_preserving_split(self.terminal_size);
        }
        self.resize_tab_panes_for_font_scale(&tab);
        self.flush_connected_tab_resize(true);
    }

    pub(crate) fn flush_connected_tab_resize(&self, preserve_split: bool) {
        // A split drag is a single geometry transaction. Sending the
        // intermediate font-aware pane sizes as ordinary ResizePanes requests
        // makes the server rebuild its split tree from each partial update.
        // Keep those sizes local until mouse release sends one ResizeSplit.
        if connected_split_drag_active(self.mux_window_id) {
            return;
        }
        if !connected_tab_resize_has_pending(self.mux_window_id) {
            return;
        }

        let mux_window_id = self.mux_window_id;
        if !CONNECTED_TAB_RESIZE_FLUSHES
            .lock()
            .expect("CONNECTED_TAB_RESIZE_FLUSHES mutex poisoned")
            .insert(mux_window_id)
        {
            return;
        }

        if pane_font_trace_enabled() {
            log::info!(
                target: "pane_font_trace",
                "gui flush-task spawn window={} preserve_split={}",
                mux_window_id,
                preserve_split
            );
            if std::env::var_os("WEZTERM_PANE_FONT_TRACE_STACKS").is_some() {
                log::info!(
                    target: "pane_font_trace",
                    "gui flush-task spawn stack:\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
            }
        }

        promise::spawn::spawn(async move {
            loop {
                smol::Timer::after(Duration::from_millis(16)).await;

                if connected_split_drag_active(mux_window_id) {
                    break;
                }

                let Some((batch_sender, batch)) =
                    take_connected_tab_resize_batch(mux_window_id, preserve_split)
                else {
                    break;
                };

                if pane_font_trace_enabled() {
                    log::info!(
                        target: "pane_font_trace",
                        "gui flush-task begin window={} panes={}",
                        mux_window_id,
                        batch.len()
                    );
                }

                if let Some(client_pane) =
                    batch_sender.downcast_ref::<wezterm_client::pane::ClientPane>()
                {
                    if let Err(err) = client_pane.flush_pending_tab_resize_now(batch).await {
                        log::error!("failed to flush final tab resize batch: {err:#}");
                    } else if pane_font_trace_enabled() {
                        log::info!(target: "pane_font_trace", "gui flush-task done");
                    }
                } else if pane_font_trace_enabled() {
                    log::error!(target: "pane_font_trace", "gui flush-task no client pane sender");
                }
            }

            CONNECTED_TAB_RESIZE_FLUSHES
                .lock()
                .expect("CONNECTED_TAB_RESIZE_FLUSHES mutex poisoned")
                .remove(&mux_window_id);

            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub(crate) fn begin_connected_split_resize(&self) {
        let inserted = CONNECTED_SPLIT_DRAGS
            .lock()
            .expect("CONNECTED_SPLIT_DRAGS mutex poisoned")
            .insert(self.mux_window_id);
        if inserted && pane_font_trace_enabled() {
            log::info!(
                target: "pane_font_trace",
                "gui split-drag begin window={}",
                self.mux_window_id
            );
        }
    }

    pub(crate) fn connected_split_resize_active(&self) -> bool {
        connected_split_drag_active(self.mux_window_id)
    }

    pub(crate) fn flush_connected_split_resize_at(
        &self,
        split_index: usize,
        start_position: usize,
    ) {
        let resize = (|| {
            let tab = Mux::get().get_active_tab_for_window(self.mux_window_id)?;
            let split = tab.iter_splits().into_iter().nth(split_index)?;
            let position = match split.direction {
                mux::tab::SplitDirection::Horizontal => split.left,
                mux::tab::SplitDirection::Vertical => split.top,
            };
            let delta = position as isize - start_position as isize;

            // Mouse motion only updates the split tree. Perform the expensive
            // terminal reflow once, using the final pixel rectangles, before
            // collecting the authoritative release batch.
            self.resize_tab_panes_for_font_scale(&tab);
            let (batch_sender, batch) = take_connected_tab_resize_batch(self.mux_window_id, true)?;
            Some((batch_sender, batch, delta))
        })();

        CONNECTED_SPLIT_DRAGS
            .lock()
            .expect("CONNECTED_SPLIT_DRAGS mutex poisoned")
            .remove(&self.mux_window_id);

        let Some((batch_sender, batch, delta)) = resize else {
            if pane_font_trace_enabled() {
                log::info!(
                    target: "pane_font_trace",
                    "gui split-drag release window={} split={} no-pending-batch",
                    self.mux_window_id,
                    split_index
                );
            }
            return;
        };

        if pane_font_trace_enabled() {
            log::info!(
                target: "pane_font_trace",
                "gui split-drag release window={} split={} delta={} panes={}",
                self.mux_window_id,
                split_index,
                delta,
                batch.len()
            );
        }

        promise::spawn::spawn(async move {
            if let Some(client_pane) =
                batch_sender.downcast_ref::<wezterm_client::pane::ClientPane>()
            {
                if let Err(err) = client_pane
                    .flush_pending_split_resize_now(split_index, delta, batch)
                    .await
                {
                    log::error!("failed to synchronize connected split resize: {err:#}");
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn sync_tab_pane_font_scales_from_mux(&mut self, tab_id: mux::tab::TabId) {
        let Some(tab) = Mux::get().get_tab(tab_id) else {
            return;
        };

        let mut changed = false;
        for pos in tab.iter_panes_ignoring_zoom() {
            let pane_id = pos.pane.pane_id();
            let font_scale = match normalize_font_scale(pos.pane.font_scale()) {
                Ok(font_scale) => font_scale,
                Err(err) => {
                    log::error!("invalid mux font scale for pane {}: {:#}", pane_id, err);
                    None
                }
            };
            let prior = self
                .pane_state
                .borrow()
                .get(&pane_id)
                .and_then(|state| state.font_scale);

            if prior != font_scale {
                self.pane_state(pane_id).font_scale = font_scale;
                changed = true;
            }
        }

        if changed {
            self.shape_generation += 1;
            self.shape_cache.borrow_mut().clear();
            self.line_to_ele_shape_cache.borrow_mut().clear();
            self.line_quad_cache.borrow_mut().clear();
            self.quad_generation += 1;
        }
    }

    fn invalidate_pane_font_scale_outputs(&mut self) {
        self.shape_generation += 1;
        self.shape_cache.borrow_mut().clear();
        self.line_to_ele_shape_cache.borrow_mut().clear();
        self.line_quad_cache.borrow_mut().clear();
        self.quad_generation += 1;

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn set_pane_font_scale_state(&mut self, pane: &Arc<dyn Pane>, font_scale: Option<f64>) -> bool {
        let pane_id = pane.pane_id();
        let new_scale = font_scale.unwrap_or(1.0);
        let font_size = self.config.font_size * new_scale;
        let theoretical_height = font_size * self.dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to set pane {} to unreasonably small font scale {}; font_height={}",
                pane_id,
                new_scale,
                theoretical_height
            );
            return false;
        }

        let font_scale = match normalize_font_scale(font_scale) {
            Ok(font_scale) => font_scale,
            Err(err) => {
                log::warn!("refusing to set pane {} font scale: {:#}", pane_id, err);
                return false;
            }
        };
        self.pane_state(pane_id).font_scale = font_scale;
        if let Err(err) = pane.set_font_scale(font_scale) {
            log::error!(
                "failed to update pane {} font scale in mux: {:#}",
                pane_id,
                err
            );
        }
        true
    }

    fn set_pane_font_scale(&mut self, pane: &Arc<dyn Pane>, font_scale: Option<f64>) {
        if !self.set_pane_font_scale_state(pane, font_scale) {
            return;
        }

        self.resize_active_tab_panes_for_font_scale();
        self.flush_connected_tab_resize(true);
        self.invalidate_pane_font_scale_outputs();
    }

    fn update_window_pane_font_sizes(
        &mut self,
        scale_for_pane: impl Fn(&Self, PaneId) -> Option<f64>,
    ) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };

        let mut seen = HashSet::new();
        let mut changed = false;
        let tabs: Vec<_> = window.iter().collect();

        for tab in &tabs {
            for pos in tab.iter_panes_ignoring_zoom() {
                let pane_id = pos.pane.pane_id();
                if !seen.insert(pane_id) {
                    continue;
                }
                changed |= self.set_pane_font_scale_state(&pos.pane, scale_for_pane(self, pane_id));
            }
        }

        if !changed {
            return;
        }

        for tab in tabs {
            self.resize_tab_panes_for_font_scale(&tab);
        }
        self.flush_connected_tab_resize(true);
        self.invalidate_pane_font_scale_outputs();
    }

    fn adjust_window_pane_font_sizes(&mut self, factor: f64) {
        self.update_window_pane_font_sizes(|this, pane_id| {
            Some(this.pane_font_scale(pane_id) * factor)
        });
    }

    pub fn decrease_pane_font_size(&mut self, pane: &Arc<dyn Pane>) {
        self.set_pane_font_scale(pane, Some(self.pane_font_scale(pane.pane_id()) / 1.1));
    }

    pub fn increase_pane_font_size(&mut self, pane: &Arc<dyn Pane>) {
        self.set_pane_font_scale(pane, Some(self.pane_font_scale(pane.pane_id()) * 1.1));
    }

    pub fn reset_pane_font_size(&mut self, pane: &Arc<dyn Pane>) {
        self.set_pane_font_scale(pane, None);
    }

    pub fn decrease_window_pane_font_size(&mut self) {
        self.adjust_window_pane_font_sizes(1.0 / 1.1);
    }

    pub fn increase_window_pane_font_size(&mut self) {
        self.adjust_window_pane_font_sizes(1.1);
    }

    pub fn reset_window_pane_font_size(&mut self) {
        self.update_window_pane_font_sizes(|_, _| None);
    }

    pub fn set_window_size(&mut self, size: TerminalSize, window: &Window) -> anyhow::Result<()> {
        let config = &self.config;
        let fontconfig = Rc::new(FontConfiguration::new(
            Some(config.clone()),
            self.dimensions.dpi,
        )?);
        let render_metrics = RenderMetrics::new(&fontconfig)?;

        let terminal_size = TerminalSize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: (render_metrics.cell_size.width as usize * size.cols),
            pixel_height: (render_metrics.cell_size.height as usize * size.rows),
            dpi: size.dpi,
        };

        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        let tab_bar_height = if show_tab_bar {
            self.tab_bar_pixel_height()? as usize
        } else {
            0
        };

        let h_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_width as f32,
            pixel_cell: render_metrics.cell_size.width as f32,
        };
        let v_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_height as f32,
            pixel_cell: render_metrics.cell_size.height as f32,
        };
        let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
        let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
        let padding_bottom = config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;

        let dimensions = Dimensions {
            pixel_width: ((terminal_size.cols as usize * render_metrics.cell_size.width as usize)
                + padding_left
                + effective_right_padding(&config, h_context)),
            pixel_height: ((terminal_size.rows as usize * render_metrics.cell_size.height as usize)
                + padding_top
                + padding_bottom) as usize
                + tab_bar_height,
            dpi: self.dimensions.dpi,
        };

        self.apply_scale_change(&dimensions, 1.0);
        self.apply_dimensions(
            &dimensions,
            Some(RowsAndCols {
                rows: size.rows as usize,
                cols: size.cols as usize,
            }),
            window,
        );
        Ok(())
    }

    pub fn reset_font_and_window_size(&mut self, window: &Window) -> anyhow::Result<()> {
        let size = self.config.initial_size(
            self.dimensions.dpi as u32,
            Some(crate::cell_pixel_dims(
                &self.config,
                self.dimensions.dpi as f64,
            )?),
        );
        self.set_window_size(size, window)
    }

    pub fn effective_right_padding(&self, config: &ConfigHandle) -> usize {
        effective_right_padding(
            config,
            DimensionContext {
                pixel_cell: self.render_metrics.cell_size.width as f32,
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_width as f32,
            },
        )
    }
}

/// Computes the effective padding for the RHS.
/// This is needed because the default is 0, but if the user has
/// enabled the scroll bar then they will expect it to have a reasonable
/// size unless they've specified differently.
pub fn effective_right_padding(config: &ConfigHandle, context: DimensionContext) -> usize {
    if config.enable_scroll_bar && config.window_padding.right.is_zero() {
        context.pixel_cell as usize
    } else {
        config.window_padding.right.evaluate_as_pixels(context) as usize
    }
}

#[cfg(test)]
mod test {
    use super::connected_tab_needs_physical_resize;
    use wezterm_term::TerminalSize;

    fn size(rows: usize, cols: usize, pixel_width: usize, pixel_height: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
            dpi: 96,
        }
    }

    #[test]
    fn mixed_font_cell_counts_do_not_resize_same_physical_tab() {
        let mixed_font_tab = size(34, 93, 780, 722);
        let base_font_window = size(38, 78, 780, 722);

        assert!(!connected_tab_needs_physical_resize(
            mixed_font_tab,
            base_font_window
        ));
    }

    #[test]
    fn connected_tab_resizes_when_physical_extent_changes() {
        let tab = size(38, 78, 780, 722);
        let wider_window = size(38, 79, 790, 722);
        let mut higher_dpi_window = tab;
        higher_dpi_window.dpi = 144;

        assert!(connected_tab_needs_physical_resize(tab, wider_window));
        assert!(connected_tab_needs_physical_resize(tab, higher_dpi_window));
    }

    #[test]
    fn connected_tab_ignores_sub_cell_split_rounding() {
        let rounded_remote_tab = size(32, 107, 1101, 739);
        let base_font_window = size(32, 110, 1100, 736);

        assert!(!connected_tab_needs_physical_resize(
            rounded_remote_tab,
            base_font_window
        ));
    }
}

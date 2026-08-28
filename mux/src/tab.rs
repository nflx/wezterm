use crate::domain::DomainId;
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{Mux, MuxNotification, WindowId};
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use parking_lot::Mutex;
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;
use wezterm_term::{StableRowIndex, TerminalSize};

pub type Tree = bintree::Tree<Arc<dyn Pane>, SplitDirectionAndSize>;
pub type Cursor = bintree::Cursor<Arc<dyn Pane>, SplitDirectionAndSize>;

static TAB_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type TabId = usize;

#[derive(Default)]
struct Recency {
    count: usize,
    by_idx: HashMap<usize, usize>,
}

impl Recency {
    fn tag(&mut self, idx: usize) {
        self.by_idx.insert(idx, self.count);
        self.count += 1;
    }

    fn score(&self, idx: usize) -> usize {
        self.by_idx.get(&idx).copied().unwrap_or(0)
    }
}

struct TabInner {
    id: TabId,
    pane: Option<Tree>,
    size: TerminalSize,
    size_before_zoom: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    title: String,
    recency: Recency,
    hidden_left_sidebar: Option<HiddenLeftSidebar>,
    hidden_right_sidebar: Option<HiddenRightSidebar>,
}

struct HiddenLeftSidebar {
    tree: Tree,
    split: SplitDirectionAndSize,
}

struct HiddenRightSidebar {
    tree: Tree,
    split: SplitDirectionAndSize,
    right_spine_depth: usize,
}

/// A Tab is a container of Panes
pub struct Tab {
    inner: Mutex<TabInner>,
    tab_id: TabId,
}

#[derive(Clone)]
pub struct PositionedPane {
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in pixels.
    pub pixel_left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in pixels.
    pub pixel_top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
    /// The pane instance
    pub pane: Arc<dyn Pane>,
}

impl std::fmt::Debug for PositionedPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedPane")
            .field("index", &self.index)
            .field("is_active", &self.is_active)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pane_id", &self.pane.pane_id())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// The size is of the (first, second) child of the split
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
    #[serde(default)]
    divider_pixel_width: usize,
    #[serde(default)]
    divider_pixel_height: usize,
    #[serde(default)]
    preferred_first: usize,
    #[serde(default)]
    preferred_second: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitRequest {
    pub direction: SplitDirection,
    /// Whether the newly created item will be in the second part
    /// of the split (right/bottom)
    pub target_is_second: bool,
    /// Split across the top of the tab rather than the active pane
    pub top_level: bool,
    /// The size of the new item
    pub size: SplitSize,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::default(),
        }
    }
}

impl SplitDirectionAndSize {
    pub(crate) fn from_snapshot(
        direction: SplitDirection,
        first: TerminalSize,
        second: TerminalSize,
    ) -> Self {
        Self {
            direction,
            first,
            second,
            divider_pixel_width: 0,
            divider_pixel_height: 0,
            preferred_first: match direction {
                SplitDirection::Horizontal => first.cols,
                SplitDirection::Vertical => first.rows,
            },
            preferred_second: match direction {
                SplitDirection::Horizontal => second.cols,
                SplitDirection::Vertical => second.rows,
            },
        }
    }

    fn first_axis_size(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.first.cols,
            SplitDirection::Vertical => self.first.rows,
        }
    }

    fn second_axis_size(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.second.cols,
            SplitDirection::Vertical => self.second.rows,
        }
    }

    fn preferred_first_axis_size(&self) -> usize {
        if self.preferred_first == 0 {
            self.first_axis_size()
        } else {
            self.preferred_first
        }
    }

    fn preferred_second_axis_size(&self) -> usize {
        if self.preferred_second == 0 {
            self.second_axis_size()
        } else {
            self.preferred_second
        }
    }

    fn set_preferred_from_current(&mut self) {
        self.preferred_first = self.first_axis_size();
        self.preferred_second = self.second_axis_size();
    }

    fn divider_pixel_width(&self) -> usize {
        if self.divider_pixel_width == 0 {
            self.first.pixel_width / self.first.cols.max(1)
        } else {
            self.divider_pixel_width
        }
    }

    fn divider_pixel_height(&self) -> usize {
        if self.divider_pixel_height == 0 {
            self.first.pixel_height / self.first.rows.max(1)
        } else {
            self.divider_pixel_height
        }
    }

    fn top_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => self.first.rows.saturating_add(1),
        }
    }

    fn left_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.first.cols.saturating_add(1),
            SplitDirection::Vertical => 0,
        }
    }

    fn pixel_top_of_second(&self, cell_dimensions: TerminalSize) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => self.first.pixel_height.saturating_add(
                self.divider_pixel_height()
                    .max(cell_dimensions.pixel_height),
            ),
        }
    }

    fn pixel_left_of_second(&self, cell_dimensions: TerminalSize) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self
                .first
                .pixel_width
                .saturating_add(self.divider_pixel_width().max(cell_dimensions.pixel_width)),
            SplitDirection::Vertical => 0,
        }
    }

    pub fn width(&self) -> usize {
        if self.direction == SplitDirection::Horizontal {
            self.first
                .cols
                .saturating_add(self.second.cols)
                .saturating_add(1)
        } else {
            self.first.cols
        }
    }

    pub fn height(&self) -> usize {
        if self.direction == SplitDirection::Vertical {
            self.first
                .rows
                .saturating_add(self.second.rows)
                .saturating_add(1)
        } else {
            self.first.rows
        }
    }

    pub fn size(&self) -> TerminalSize {
        let rows = self.height();
        let cols = self.width();
        let cell_width = self.first.pixel_width / self.first.cols.max(1);
        let cell_height = self.first.pixel_height / self.first.rows.max(1);

        TerminalSize {
            rows,
            cols,
            pixel_height: match self.direction {
                SplitDirection::Horizontal => self.first.pixel_height.max(self.second.pixel_height),
                SplitDirection::Vertical => self
                    .first
                    .pixel_height
                    .saturating_add(self.second.pixel_height)
                    .saturating_add(self.divider_pixel_height().max(cell_height)),
            },
            pixel_width: match self.direction {
                SplitDirection::Horizontal => self
                    .first
                    .pixel_width
                    .saturating_add(self.second.pixel_width)
                    .saturating_add(self.divider_pixel_width().max(cell_width)),
                SplitDirection::Vertical => self.first.pixel_width.max(self.second.pixel_width),
            },
            dpi: self.first.dpi,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PositionedSplit {
    /// The topological node index that can be used to reference this split
    pub index: usize,
    pub direction: SplitDirection,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in pixels.
    pub pixel_left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub top: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in pixels.
    pub pixel_top: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be
    pub size: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be, in pixels.
    pub pixel_size: usize,
    /// Width of the divider cell between split children, in pixels.
    pub divider_pixel_width: usize,
    /// Height of the divider cell between split children, in pixels.
    pub divider_pixel_height: usize,
}

fn is_pane(pane: &Arc<dyn Pane>, other: &Option<&Arc<dyn Pane>>) -> bool {
    if let Some(other) = other {
        other.pane_id() == pane.pane_id()
    } else {
        false
    }
}

fn pane_tree(
    tree: &Tree,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
    left_px: usize,
    top_px: usize,
    cell_dimensions: TerminalSize,
) -> PaneNode {
    match tree {
        Tree::Empty => PaneNode::Empty,
        Tree::Node { left, right, data } => {
            let data = data.unwrap();
            PaneNode::Split {
                left: Box::new(pane_tree(
                    &*left,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    left_col,
                    top_row,
                    left_px,
                    top_px,
                    cell_dimensions,
                )),
                right: Box::new(pane_tree(
                    &*right,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    if data.direction == SplitDirection::Vertical {
                        left_col
                    } else {
                        left_col + data.left_of_second()
                    },
                    if data.direction == SplitDirection::Horizontal {
                        top_row
                    } else {
                        top_row + data.top_of_second()
                    },
                    left_px + data.pixel_left_of_second(cell_dimensions),
                    top_px + data.pixel_top_of_second(cell_dimensions),
                    cell_dimensions,
                )),
                node: data,
            }
        }
        Tree::Leaf(pane) => {
            let dims = pane.get_dimensions();
            let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
            let cursor_pos = pane.get_cursor_position();

            PaneNode::Leaf(PaneEntry {
                window_id,
                tab_id,
                pane_id: pane.pane_id(),
                title: pane.get_title(),
                is_active_pane: is_pane(pane, &active),
                is_zoomed_pane: is_pane(pane, &zoomed),
                size: TerminalSize {
                    cols: dims.cols,
                    rows: dims.viewport_rows,
                    pixel_height: dims.pixel_height,
                    pixel_width: dims.pixel_width,
                    dpi: dims.dpi,
                },
                working_dir: working_dir.map(Into::into),
                workspace: workspace.to_string(),
                cursor_pos,
                physical_top: dims.physical_top,
                left_col,
                top_row,
                left_px,
                top_px,
                font_scale: pane.font_scale(),
                tty_name: pane.tty_name(),
                tmux_connection_state: Mux::try_get()
                    .and_then(|mux| mux.get_domain(pane.domain_id()))
                    .and_then(|domain| {
                        domain
                            .downcast_ref::<crate::tmux::TmuxDomain>()
                            .map(|tmux| tmux.connection_state())
                    }),
            })
        }
    }
}

fn build_from_pane_tree<F>(
    tree: bintree::Tree<PaneEntry, SplitDirectionAndSize>,
    active: &mut Option<Arc<dyn Pane>>,
    zoomed: &mut Option<Arc<dyn Pane>>,
    make_pane: &mut F,
) -> Tree
where
    F: FnMut(PaneEntry) -> Arc<dyn Pane>,
{
    match tree {
        bintree::Tree::Empty => Tree::Empty,
        bintree::Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(build_from_pane_tree(*left, active, zoomed, make_pane)),
            right: Box::new(build_from_pane_tree(*right, active, zoomed, make_pane)),
            data,
        },
        bintree::Tree::Leaf(entry) => {
            let is_zoomed_pane = entry.is_zoomed_pane;
            let is_active_pane = entry.is_active_pane;
            let pane = make_pane(entry);
            if is_zoomed_pane {
                zoomed.replace(Arc::clone(&pane));
            }
            if is_active_pane {
                active.replace(Arc::clone(&pane));
            }
            Tree::Leaf(pane)
        }
    }
}

/// Computes the minimum (x, y) size based on the panes in this portion
/// of the tree.
fn compute_min_size(tree: &mut Tree) -> (usize, usize) {
    match tree {
        Tree::Node { data: None, .. } | Tree::Empty => (1, 1),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (left_x, left_y) = compute_min_size(&mut *left);
            let (right_x, right_y) = compute_min_size(&mut *right);
            match data.direction {
                SplitDirection::Vertical => (left_x.max(right_x), left_y + right_y + 1),
                SplitDirection::Horizontal => (left_x + right_x + 1, left_y.max(right_y)),
            }
        }
        Tree::Leaf(_) => (1, 1),
    }
}

fn count_tree_leaves(tree: &Tree) -> usize {
    match tree {
        Tree::Empty => 0,
        Tree::Leaf(_) => 1,
        Tree::Node { left, right, .. } => count_tree_leaves(left) + count_tree_leaves(right),
    }
}

fn detach_right_sidebar_tree(
    root: Tree,
) -> Result<(Tree, Tree, SplitDirectionAndSize, usize), Tree> {
    match root {
        Tree::Node {
            left,
            right,
            data: Some(split),
        } if split.direction == SplitDirection::Horizontal => {
            let right_continues_column = matches!(
                right.as_ref(),
                Tree::Node {
                    data: Some(nested),
                    ..
                } if nested.direction == SplitDirection::Horizontal
            );
            if right_continues_column {
                match detach_right_sidebar_tree(*right) {
                    Ok((main_right, tree, sidebar_split, depth)) => Ok((
                        Tree::Node {
                            left,
                            right: Box::new(main_right),
                            data: Some(split),
                        },
                        tree,
                        sidebar_split,
                        depth + 1,
                    )),
                    Err(right) => Err(Tree::Node {
                        left,
                        right: Box::new(right),
                        data: Some(split),
                    }),
                }
            } else {
                Ok((*left, *right, split, 0))
            }
        }
        root => Err(root),
    }
}

fn restore_right_sidebar_tree(
    main: Tree,
    sidebar: Tree,
    split: SplitDirectionAndSize,
    right_spine_depth: usize,
) -> Tree {
    if right_spine_depth == 0 {
        return Tree::Node {
            left: Box::new(main),
            right: Box::new(sidebar),
            data: Some(split),
        };
    }

    match main {
        Tree::Node { left, right, data } => Tree::Node {
            left,
            right: Box::new(restore_right_sidebar_tree(
                *right,
                sidebar,
                split,
                right_spine_depth - 1,
            )),
            data,
        },
        _ => unreachable!("hidden right sidebar path must remain present in the main pane tree"),
    }
}

fn split_child_lengths(
    total: usize,
    first_current: usize,
    second_current: usize,
    first_min: usize,
    second_min: usize,
) -> (usize, usize) {
    let available = total.saturating_sub(1);
    let min_sum = first_min.saturating_add(second_min);

    if available <= min_sum {
        let first = if min_sum == 0 {
            available / 2
        } else {
            available
                .saturating_mul(first_min)
                .saturating_add(min_sum / 2)
                / min_sum
        }
        .min(available);
        return (first, available.saturating_sub(first));
    }

    let current_sum = first_current.saturating_add(second_current);
    let first = if current_sum == 0 {
        available / 2
    } else {
        available
            .saturating_mul(first_current)
            .saturating_add(current_sum / 2)
            / current_sum
    };
    let first = first
        .max(first_min)
        .min(available.saturating_sub(second_min));

    (first, available.saturating_sub(first))
}

fn terminal_size_with_pixels(
    rows: usize,
    cols: usize,
    pixel_height: usize,
    pixel_width: usize,
    dpi: u32,
) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_height,
        pixel_width,
        dpi,
    }
}

fn split_child_pixels(
    total_pixels: usize,
    first_cells: usize,
    divider_pixels: usize,
    cell_pixels: usize,
) -> (usize, usize) {
    let available_pixels = total_pixels.saturating_sub(divider_pixels);
    let first_pixels = first_cells
        .saturating_mul(cell_pixels)
        .min(available_pixels);

    (first_pixels, available_pixels.saturating_sub(first_pixels))
}

fn split_dimensions(dim: usize, request: SplitRequest) -> Option<(usize, usize)> {
    let available = dim.checked_sub(1)?;
    if available < 2 {
        return None;
    }

    let target_size = match request.size {
        SplitSize::Cells(n) => n,
        SplitSize::Percent(n) => dim.saturating_mul(n as usize) / 100,
    }
    .max(1)
    .min(available.saturating_sub(1));

    let remain = available.saturating_sub(target_size);

    Some(if request.target_is_second {
        (remain, target_size)
    } else {
        (target_size, remain)
    })
}

fn split_size_from_parent(
    parent: TerminalSize,
    direction: SplitDirection,
    first_rows: usize,
    second_rows: usize,
    first_cols: usize,
    second_cols: usize,
    cell_dimensions: &TerminalSize,
) -> SplitDirectionAndSize {
    let (first_pixel_height, second_pixel_height) = if direction == SplitDirection::Vertical {
        split_child_pixels(
            parent.pixel_height,
            first_rows,
            cell_dimensions.pixel_height,
            cell_dimensions.pixel_height,
        )
    } else {
        (parent.pixel_height, parent.pixel_height)
    };
    let (first_pixel_width, second_pixel_width) = if direction == SplitDirection::Horizontal {
        split_child_pixels(
            parent.pixel_width,
            first_cols,
            cell_dimensions.pixel_width,
            cell_dimensions.pixel_width,
        )
    } else {
        (parent.pixel_width, parent.pixel_width)
    };

    SplitDirectionAndSize {
        direction,
        first: TerminalSize {
            rows: first_rows,
            cols: first_cols,
            pixel_height: first_pixel_height,
            pixel_width: first_pixel_width,
            dpi: cell_dimensions.dpi,
        },
        second: TerminalSize {
            rows: second_rows,
            cols: second_cols,
            pixel_height: second_pixel_height,
            pixel_width: second_pixel_width,
            dpi: cell_dimensions.dpi,
        },
        preferred_first: match direction {
            SplitDirection::Horizontal => first_cols,
            SplitDirection::Vertical => first_rows,
        },
        preferred_second: match direction {
            SplitDirection::Horizontal => second_cols,
            SplitDirection::Vertical => second_rows,
        },
        divider_pixel_width: cell_dimensions.pixel_width,
        divider_pixel_height: cell_dimensions.pixel_height,
    }
}

fn resize_tree_to_size(tree: &mut Tree, size: &TerminalSize, cell_dimensions: &TerminalSize) {
    match tree {
        Tree::Empty | Tree::Leaf(_) => {}
        Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => match data.direction {
            SplitDirection::Horizontal => {
                let (first_min, _) = compute_min_size(&mut *left);
                let (second_min, _) = compute_min_size(&mut *right);
                let (first_cols, second_cols) = split_child_lengths(
                    size.cols,
                    data.preferred_first_axis_size(),
                    data.preferred_second_axis_size(),
                    first_min,
                    second_min,
                );
                let (first_pixel_width, second_pixel_width) = split_child_pixels(
                    size.pixel_width,
                    first_cols,
                    cell_dimensions.pixel_width,
                    cell_dimensions.pixel_width,
                );
                data.divider_pixel_width = cell_dimensions.pixel_width;
                data.divider_pixel_height = cell_dimensions.pixel_height;

                data.first = terminal_size_with_pixels(
                    size.rows,
                    first_cols,
                    size.pixel_height,
                    first_pixel_width,
                    cell_dimensions.dpi,
                );
                data.second = terminal_size_with_pixels(
                    size.rows,
                    second_cols,
                    size.pixel_height,
                    second_pixel_width,
                    cell_dimensions.dpi,
                );

                resize_tree_to_size(&mut *left, &data.first, cell_dimensions);
                resize_tree_to_size(&mut *right, &data.second, cell_dimensions);
            }
            SplitDirection::Vertical => {
                let (_, first_min) = compute_min_size(&mut *left);
                let (_, second_min) = compute_min_size(&mut *right);
                let (first_rows, second_rows) = split_child_lengths(
                    size.rows,
                    data.preferred_first_axis_size(),
                    data.preferred_second_axis_size(),
                    first_min,
                    second_min,
                );
                let (first_pixel_height, second_pixel_height) = split_child_pixels(
                    size.pixel_height,
                    first_rows,
                    cell_dimensions.pixel_height,
                    cell_dimensions.pixel_height,
                );
                data.divider_pixel_width = cell_dimensions.pixel_width;
                data.divider_pixel_height = cell_dimensions.pixel_height;

                data.first = terminal_size_with_pixels(
                    first_rows,
                    size.cols,
                    first_pixel_height,
                    size.pixel_width,
                    cell_dimensions.dpi,
                );
                data.second = terminal_size_with_pixels(
                    second_rows,
                    size.cols,
                    second_pixel_height,
                    size.pixel_width,
                    cell_dimensions.dpi,
                );

                resize_tree_to_size(&mut *left, &data.first, cell_dimensions);
                resize_tree_to_size(&mut *right, &data.second, cell_dimensions);
            }
        },
    }
}

fn apply_sizes_from_splits_impl(
    tree: &Tree,
    size: &TerminalSize,
    preserve_split: bool,
    defer_preserve_split: bool,
) {
    match tree {
        Tree::Empty => return,
        Tree::Node { data: None, .. } => return,
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            apply_sizes_from_splits_impl(&*left, &data.first, preserve_split, defer_preserve_split);
            apply_sizes_from_splits_impl(
                &*right,
                &data.second,
                preserve_split,
                defer_preserve_split,
            );
        }
        Tree::Leaf(pane) => {
            if preserve_split {
                if defer_preserve_split {
                    pane.resize_preserving_split_for_split_drag(*size).ok();
                } else {
                    pane.resize_preserving_split(*size).ok();
                }
            } else {
                pane.resize(*size).ok();
            }
        }
    }
}

fn apply_sizes_from_splits(tree: &Tree, size: &TerminalSize) {
    apply_sizes_from_splits_impl(tree, size, false, false);
}

fn apply_sizes_from_splits_preserving_split(tree: &Tree, size: &TerminalSize) {
    apply_sizes_from_splits_impl(tree, size, true, false);
}

fn apply_sizes_from_splits_preserving_split_for_split_drag(tree: &Tree, size: &TerminalSize) {
    apply_sizes_from_splits_impl(tree, size, true, true);
}

fn cell_dimensions(size: &TerminalSize) -> TerminalSize {
    TerminalSize {
        rows: 1,
        cols: 1,
        pixel_width: size.pixel_width / size.cols,
        pixel_height: size.pixel_height / size.rows,
        dpi: size.dpi,
    }
}

impl Tab {
    pub fn new(size: &TerminalSize) -> Self {
        let inner = TabInner::new(size);
        let tab_id = inner.id;
        Self {
            inner: Mutex::new(inner),
            tab_id,
        }
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.clone()
    }

    pub fn set_title(&self, title: &str) {
        let mut inner = self.inner.lock();
        if inner.title != title {
            inner.title = title.to_string();
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::TabTitleChanged {
                    tab_id: inner.id,
                    title: title.to_string(),
                })
            });
        }
    }

    /// Called by the multiplexer client when building a local tab to
    /// mirror a remote tab.  The supplied `root` is the information
    /// about our counterpart in the remote server.
    /// This method builds a local tree based on the remote tree which
    /// then replaces the local tree structure.
    ///
    /// The `make_pane` function is provided by the caller, and its purpose
    /// is to lookup an existing Pane that corresponds to the provided
    /// PaneEntry, or to create a new Pane from that entry.
    /// make_pane is expected to add the pane to the mux if it creates
    /// a new pane, otherwise the pane won't poll/update in the GUI.
    pub fn sync_with_pane_tree<F>(&self, size: TerminalSize, root: PaneNode, make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        self.inner
            .lock()
            .sync_with_pane_tree(size, root, make_pane, true)
    }

    /// Synchronize a remotely supplied topology while retaining the dimensions
    /// carried by each remote pane. Split-tree cell dimensions use the tab's
    /// base font metrics and are not the terminal dimensions of panes with an
    /// individual font scale.
    pub fn sync_with_pane_tree_preserving_pane_sizes<F>(
        &self,
        size: TerminalSize,
        root: PaneNode,
        make_pane: F,
    ) where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        self.inner
            .lock()
            .sync_with_pane_tree(size, root, make_pane, false)
    }

    pub fn codec_pane_tree(&self) -> PaneNode {
        self.inner.lock().codec_pane_tree()
    }

    /// Returns a count of how many panes are in this tab
    pub fn count_panes(&self) -> Option<usize> {
        self.inner.try_lock().map(|mut inner| inner.count_panes())
    }

    /// Sets the zoom state, returns the prior state
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        self.inner.lock().set_zoomed(zoomed)
    }

    pub fn toggle_zoom(&self) {
        self.inner.lock().toggle_zoom()
    }

    pub fn left_sidebar_hidden(&self) -> bool {
        self.inner.lock().hidden_left_sidebar.is_some()
    }

    pub fn set_left_sidebar_hidden(&self, hidden: bool) -> anyhow::Result<()> {
        self.inner.lock().set_left_sidebar_hidden(hidden)
    }

    pub fn right_sidebar_hidden(&self) -> bool {
        self.inner.lock().hidden_right_sidebar.is_some()
    }

    pub fn set_right_sidebar_hidden(&self, hidden: bool) -> anyhow::Result<()> {
        self.inner.lock().set_right_sidebar_hidden(hidden)
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        self.inner.lock().contains_pane(pane)
    }

    pub fn iter_panes(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes()
    }

    pub fn iter_panes_ignoring_zoom(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes_ignoring_zoom()
    }

    pub fn rotate_counter_clockwise(&self) {
        self.inner.lock().rotate_counter_clockwise()
    }

    pub fn rotate_clockwise(&self) {
        self.inner.lock().rotate_clockwise()
    }

    pub fn iter_splits(&self) -> Vec<PositionedSplit> {
        self.inner.lock().iter_splits()
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn get_size(&self) -> TerminalSize {
        self.inner.lock().get_size()
    }

    /// Apply the new size of the tab to the panes contained within.
    /// The delta between the current and the new size is computed,
    /// and is distributed between the splits.  For small resizes
    /// this algorithm biases towards adjusting the left/top nodes
    /// first.  For large resizes this tends to proportionally adjust
    /// the relative sizes of the elements in a split.
    pub fn resize(&self, size: TerminalSize) {
        self.inner.lock().resize(size)
    }

    pub fn resize_preserving_split(&self, size: TerminalSize) {
        self.inner.lock().resize_preserving_split(size)
    }

    /// Called when running in the mux server after an individual pane
    /// has been resized.
    /// Because the split manipulation happened on the GUI we "lost"
    /// the information that would have allowed us to call resize_split_by()
    /// and instead need to back-infer the split size information.
    /// We rely on the client to have resized (or be in the process
    /// of resizing) affected panes consistently with its own Tab
    /// tree model.
    /// This method does a simple tree walk to the leaves to back-propagate
    /// the size of the panes up to their containing node split data.
    /// Without this step, disconnecting and reconnecting would cause
    /// the GUI to use stale size information for the window it spawns
    /// to attach this tab.
    pub fn rebuild_splits_sizes_from_contained_panes(&self) {
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes(true)
    }

    pub fn rebuild_splits_sizes_from_contained_panes_silently(&self) {
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes(false)
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propogated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        self.inner.lock().resize_split_by(split_index, delta)
    }

    pub fn resize_split_by_preserving_split(&self, split_index: usize, delta: isize) {
        self.inner
            .lock()
            .resize_split_by_preserving_split(split_index, delta)
    }

    /// Adjusts the size of the active pane in the specified direction
    /// by the specified amount.
    pub fn adjust_pane_size(&self, direction: PaneDirection, amount: usize) {
        self.inner.lock().adjust_pane_size(direction, amount)
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        self.inner.lock().activate_pane_direction(direction)
    }

    /// Returns an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn get_pane_direction(&self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        self.inner.lock().get_pane_direction(direction, ignore_zoom)
    }

    pub fn prune_dead_panes(&self) -> bool {
        self.inner.lock().prune_dead_panes(false)
    }

    pub fn prune_dead_panes_preserving_split(&self) -> bool {
        self.inner.lock().prune_dead_panes(true)
    }

    pub fn kill_pane(&self, pane_id: PaneId) -> bool {
        self.inner.lock().kill_pane(pane_id)
    }

    pub fn kill_panes_in_domain(&self, domain: DomainId) -> bool {
        self.inner.lock().kill_panes_in_domain(domain)
    }

    /// Remove pane from tab.
    /// The pane is still live in the mux; the intent is for the pane to
    /// be added to a different tab.
    pub fn remove_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_pane(pane_id)
    }

    pub fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        self.inner.lock().can_close_without_prompting(reason)
    }

    pub fn is_dead(&self) -> bool {
        self.inner.lock().is_dead()
    }

    pub fn get_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_active_pane()
    }

    #[allow(unused)]
    pub fn get_active_idx(&self) -> usize {
        self.inner.lock().get_active_idx()
    }

    pub fn set_active_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane(pane, true)
    }

    /// Apply a focus change received from a mux notification without
    /// publishing the same notification back onto the mux bus.
    pub fn reconcile_active_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane(pane, false)
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        self.inner.lock().set_active_idx(pane_index)
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().assign_pane(pane)
    }

    /// Swap the active pane with the specified pane_index
    pub fn swap_active_with_index(&self, pane_index: usize, keep_focus: bool) -> Option<()> {
        self.inner
            .lock()
            .swap_active_with_index(pane_index, keep_focus)
    }

    /// Detach a pane from its current branch and insert it beside another pane.
    pub fn reposition_pane(
        &self,
        pane_id: PaneId,
        target_pane_id: PaneId,
        request: SplitRequest,
    ) -> anyhow::Result<()> {
        self.inner
            .lock()
            .reposition_pane(pane_id, target_pane_id, request)
    }

    /// Atomically replace the pane topology while retaining the supplied pane
    /// objects and selecting the requested stable pane as active.
    ///
    /// The candidate tree is fully validated before the live tree is changed.
    #[allow(dead_code)] // Used by the tmux atomic apply phase in the next slice.
    pub(crate) fn replace_pane_tree(
        &self,
        tree: Tree,
        active_pane_id: PaneId,
    ) -> anyhow::Result<()> {
        self.inner
            .lock()
            .replace_pane_tree(tree, active_pane_id, true)
    }

    pub(crate) fn validate_pane_tree(tree: &Tree, active_pane_id: PaneId) -> anyhow::Result<()> {
        TabInner::validate_pane_tree(tree, active_pane_id).map(|_| ())
    }

    pub(crate) fn replace_pane_tree_silently(
        &self,
        tree: Tree,
        active_pane_id: PaneId,
    ) -> anyhow::Result<()> {
        self.inner
            .lock()
            .replace_pane_tree(tree, active_pane_id, false)
    }

    /// Snapshot the current binary pane tree, retaining Arc identity and the
    /// exact local split data used for pixel layout.
    #[allow(dead_code)] // Consumed by tmux subtree ratio reconciliation.
    pub(crate) fn snapshot_pane_tree(&self) -> Tree {
        fn clone_tree(tree: &Tree) -> Tree {
            match tree {
                Tree::Empty => Tree::Empty,
                Tree::Leaf(pane) => Tree::Leaf(Arc::clone(pane)),
                Tree::Node { left, right, data } => Tree::Node {
                    left: Box::new(clone_tree(left)),
                    right: Box::new(clone_tree(right)),
                    data: *data,
                },
            }
        }

        let inner = self.inner.lock();
        clone_tree(
            inner
                .pane
                .as_ref()
                .expect("tab pane tree is always present"),
        )
    }

    /// Computes the size of the pane that would result if the specified
    /// pane was split in a particular direction.
    /// The intent is to call this prior to spawning the new pane so that
    /// you can create it with the correct size.
    /// May return None if the specified pane_index is invalid.
    pub fn compute_split_size(
        &self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        self.inner.lock().compute_split_size(pane_index, request)
    }

    /// Split the pane that has pane_index in the given direction and assign
    /// the right/bottom pane of the newly created split to the provided Pane
    /// instance.  Returns the resultant index of the newly inserted pane.
    /// Both the split and the inserted pane will be resized.
    pub fn split_and_insert(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane, false)
    }

    pub fn split_and_insert_preserving_split(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane, true)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: TAB_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            pane: Some(Tree::new()),
            size: *size,
            size_before_zoom: *size,
            active: 0,
            zoomed: None,
            title: String::new(),
            recency: Recency::default(),
            hidden_left_sidebar: None,
            hidden_right_sidebar: None,
        }
    }

    fn sync_with_pane_tree<F>(
        &mut self,
        size: TerminalSize,
        root: PaneNode,
        mut make_pane: F,
        resize_panes: bool,
    ) where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        let mut active = None;
        let mut zoomed = None;

        log::debug!("sync_with_pane_tree with size {:?}", size);

        let left_sidebar_hidden = root.left_sidebar_hidden();
        let right_sidebar_hidden = root.right_sidebar_hidden();
        let t = build_from_pane_tree(root.into_tree(), &mut active, &mut zoomed, &mut make_pane);
        let mut cursor = t.cursor();

        self.active = 0;
        if let Some(active) = active {
            // Resolve the active pane to its index
            let mut index = 0;
            loop {
                if let Some(pane) = cursor.leaf_mut() {
                    if active.pane_id() == pane.pane_id() {
                        // Found it
                        self.active = index;
                        self.recency.tag(index);
                        break;
                    }
                    index += 1;
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        // Didn't find it
                        cursor = c;
                        break;
                    }
                }
            }
        }
        self.pane.replace(cursor.tree());
        self.zoomed = zoomed;
        self.size = size;
        self.hidden_left_sidebar = None;
        self.hidden_right_sidebar = None;

        if left_sidebar_hidden {
            if let Err(err) = self.set_left_sidebar_hidden(true) {
                log::error!("failed to restore hidden left sidebar: {err:#}");
            }
        } else if right_sidebar_hidden {
            if let Err(err) = self.set_right_sidebar_hidden(true) {
                log::error!("failed to restore hidden right sidebar: {err:#}");
            }
        }

        if resize_panes {
            if let Some(zoomed) = &self.zoomed {
                zoomed.resize_preserving_split(size).ok();
            } else if let Some(root) = self.pane.as_mut() {
                apply_sizes_from_splits_preserving_split(root, &size);
            }
        }
        if self.zoomed.is_none() {
            Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        }

        log::debug!(
            "sync tab: {:#?} zoomed: {} {:#?}",
            size,
            self.zoomed.is_some(),
            self.iter_panes()
        );
        assert!(self.pane.is_some());
    }

    fn codec_pane_tree(&mut self) -> PaneNode {
        let mux = Mux::get();
        let tab_id = self.id;
        let window_id = match mux.window_containing_tab(tab_id) {
            Some(w) => w,
            None => {
                log::error!("no window contains tab {}", tab_id);
                return PaneNode::Empty;
            }
        };

        let workspace = match mux
            .get_window(window_id)
            .map(|w| w.get_workspace().to_string())
        {
            Some(ws) => ws,
            None => {
                log::error!("window id {} doesn't have a window!?", window_id);
                return PaneNode::Empty;
            }
        };

        let active = self.get_active_pane();
        let zoomed = self.zoomed.clone();
        let cell_dimensions = self.cell_dimensions();
        let left_was_hidden = self.hidden_left_sidebar.is_some();
        let right_was_hidden = self.hidden_right_sidebar.is_some();
        if left_was_hidden {
            self.restore_hidden_left_sidebar_tree();
        } else if right_was_hidden {
            self.restore_hidden_right_sidebar_tree();
        }
        let result = if let Some(root) = self.pane.as_ref() {
            PaneNode::TabRoot {
                root: Box::new(pane_tree(
                    root,
                    tab_id,
                    window_id,
                    active.as_ref(),
                    zoomed.as_ref(),
                    &workspace,
                    0,
                    0,
                    0,
                    0,
                    cell_dimensions,
                )),
                left_sidebar_hidden: left_was_hidden,
                right_sidebar_hidden: right_was_hidden,
            }
        } else {
            PaneNode::Empty
        };
        if left_was_hidden {
            self.detach_left_sidebar_tree()
                .expect("restoring a previously hidden left sidebar must succeed");
        } else if right_was_hidden {
            self.detach_right_sidebar_tree()
                .expect("restoring a previously hidden right sidebar must succeed");
        }
        result
    }

    fn restore_hidden_left_sidebar_tree(&mut self) {
        let Some(hidden) = self.hidden_left_sidebar.take() else {
            return;
        };
        let main = self.pane.take().unwrap_or(Tree::Empty);
        let left_count = count_tree_leaves(&hidden.tree);
        self.pane = Some(Tree::Node {
            left: Box::new(hidden.tree),
            right: Box::new(main),
            data: Some(hidden.split),
        });
        self.active = self.active.saturating_add(left_count);
    }

    fn detach_left_sidebar_tree(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.hidden_left_sidebar.is_none(),
            "left sidebar is already hidden"
        );
        let root = self.pane.take().unwrap_or(Tree::Empty);
        match root {
            Tree::Node {
                left,
                right,
                data: Some(split),
            } if split.direction == SplitDirection::Horizontal => {
                let left_count = count_tree_leaves(&left);
                self.pane = Some(*right);
                self.active = self.active.saturating_sub(left_count);
                self.hidden_left_sidebar = Some(HiddenLeftSidebar { tree: *left, split });
                Ok(())
            }
            other => {
                self.pane = Some(other);
                anyhow::bail!("the tab root must be a horizontal split to hide its left sidebar")
            }
        }
    }

    fn set_left_sidebar_hidden(&mut self, hidden: bool) -> anyhow::Result<()> {
        if hidden == self.hidden_left_sidebar.is_some() {
            return Ok(());
        }
        if hidden {
            anyhow::ensure!(
                self.hidden_right_sidebar.is_none(),
                "cannot hide both sidebars at once"
            );
            self.detach_left_sidebar_tree()?;
            if self.active > 0 {
                self.active = 0;
            }
            if self.zoomed.is_none() {
                self.resize(self.size);
            }
        } else {
            self.restore_hidden_left_sidebar_tree();
            if self.zoomed.is_none() {
                self.resize(self.size);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        Ok(())
    }

    fn restore_hidden_right_sidebar_tree(&mut self) {
        let Some(hidden) = self.hidden_right_sidebar.take() else {
            return;
        };
        let main = self.pane.take().unwrap_or(Tree::Empty);
        self.pane = Some(restore_right_sidebar_tree(
            main,
            hidden.tree,
            hidden.split,
            hidden.right_spine_depth,
        ));
    }

    fn detach_right_sidebar_tree(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.hidden_right_sidebar.is_none(),
            "right sidebar is already hidden"
        );
        let root = self.pane.take().unwrap_or(Tree::Empty);
        match detach_right_sidebar_tree(root) {
            Ok((main, tree, split, right_spine_depth)) => {
                let main_count = count_tree_leaves(&main);
                self.pane = Some(main);
                if self.active >= main_count {
                    self.active = 0;
                }
                self.hidden_right_sidebar = Some(HiddenRightSidebar {
                    tree,
                    split,
                    right_spine_depth,
                });
                Ok(())
            }
            Err(root) => {
                self.pane = Some(root);
                anyhow::bail!("the tab root must be a horizontal split to hide its right sidebar")
            }
        }
    }

    fn set_right_sidebar_hidden(&mut self, hidden: bool) -> anyhow::Result<()> {
        if hidden == self.hidden_right_sidebar.is_some() {
            return Ok(());
        }
        if hidden {
            anyhow::ensure!(
                self.hidden_left_sidebar.is_none(),
                "cannot hide both sidebars at once"
            );
            self.detach_right_sidebar_tree()?;
            if self.zoomed.is_none() {
                self.resize(self.size);
            }
        } else {
            self.restore_hidden_right_sidebar_tree();
            if self.zoomed.is_none() {
                self.resize(self.size);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        Ok(())
    }

    /// Returns a count of how many panes are in this tab
    fn count_panes(&mut self) -> usize {
        let mut count = 0;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                count += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return count;
                }
            }
        }
    }

    /// Sets the zoom state, returns the prior state
    fn set_zoomed(&mut self, zoomed: bool) -> bool {
        if self.zoomed.is_some() == zoomed {
            // Current zoom state matches intended zoom state,
            // so we have nothing to do.
            return zoomed;
        }
        self.toggle_zoom();
        !zoomed
    }

    fn toggle_zoom(&mut self) {
        let size = self.size;
        if self.zoomed.take().is_some() {
            // We were zoomed, but now we are not.
            // Re-apply the size to the panes
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(false);
            }
            self.size = self.size_before_zoom;
            self.resize(size);
        } else {
            // We weren't zoomed, but now we want to zoom.
            // Locate the active pane
            self.size_before_zoom = size;
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(true);
                pane.resize(size).ok();
                self.zoomed.replace(pane);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        fn contains(tree: &Tree, pane: PaneId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => contains(left, pane) || contains(right, pane),
                Tree::Leaf(p) => p.pane_id() == pane,
            }
        }
        match &self.pane {
            Some(root) => contains(root, pane),
            None => false,
        }
    }

    /// Walks the pane tree to produce the topologically ordered flattened
    /// list of PositionedPane instances along with their positioning information.
    fn iter_panes(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(true)
    }

    /// Like iter_panes, except that it will include all panes, regardless of
    /// whether one of them is currently zoomed.
    fn iter_panes_ignoring_zoom(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(false)
    }

    fn rotate_counter_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .first()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.postorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
    }

    fn rotate_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .last()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn iter_panes_impl(&mut self, respect_zoom_state: bool) -> Vec<PositionedPane> {
        let mut panes = vec![];

        if respect_zoom_state {
            if let Some(zoomed) = self.zoomed.as_ref() {
                let size = self.size;
                let dims = zoomed.get_dimensions();
                panes.push(PositionedPane {
                    index: 0,
                    is_active: true,
                    is_zoomed: true,
                    left: 0,
                    pixel_left: 0,
                    top: 0,
                    pixel_top: 0,
                    width: dims.cols,
                    pixel_width: size.pixel_width.into(),
                    height: dims.viewport_rows,
                    pixel_height: size.pixel_height.into(),
                    pane: Arc::clone(zoomed),
                });
                return panes;
            }
        }

        let active_idx = self.active;
        let zoomed_id = self.zoomed.as_ref().map(|p| p.pane_id());
        let root_size = self.size;
        let cell_dimensions = self.cell_dimensions();
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let index = panes.len();
                let mut left = 0usize;
                let mut pixel_left = 0usize;
                let mut top = 0usize;
                let mut pixel_top = 0usize;
                let mut parent_size = None;
                for (branch, node) in cursor.path_to_root() {
                    if let Some(node) = node {
                        if parent_size.is_none() {
                            parent_size.replace(if branch == PathBranch::IsRight {
                                node.second
                            } else {
                                node.first
                            });
                        }
                        if branch == PathBranch::IsRight {
                            top += node.top_of_second();
                            left += node.left_of_second();
                            pixel_top += node.pixel_top_of_second(cell_dimensions);
                            pixel_left += node.pixel_left_of_second(cell_dimensions);
                        }
                    }
                }

                let pane = Arc::clone(cursor.leaf_mut().unwrap());
                let dims = parent_size.unwrap_or_else(|| root_size);
                let pane_dims = pane.get_dimensions();

                panes.push(PositionedPane {
                    index,
                    is_active: index == active_idx,
                    is_zoomed: zoomed_id == Some(pane.pane_id()),
                    left,
                    pixel_left,
                    top,
                    pixel_top,
                    width: pane_dims.cols,
                    height: pane_dims.viewport_rows,
                    pixel_width: dims.pixel_width as _,
                    pixel_height: dims.pixel_height as _,
                    pane,
                });
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn iter_splits(&mut self) -> Vec<PositionedSplit> {
        let mut dividers = vec![];
        if self.zoomed.is_some() {
            return dividers;
        }

        let cell_dimensions = self.cell_dimensions();
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        loop {
            if !cursor.is_leaf() {
                let mut left = 0usize;
                let mut top = 0usize;
                let mut pixel_left = 0usize;
                let mut pixel_top = 0usize;
                for (branch, p) in cursor.path_to_root() {
                    if let Some(p) = p {
                        if branch == PathBranch::IsRight {
                            left += p.left_of_second();
                            top += p.top_of_second();
                            pixel_left += p.pixel_left_of_second(cell_dimensions);
                            pixel_top += p.pixel_top_of_second(cell_dimensions);
                        }
                    }
                }
                if let Ok(Some(node)) = cursor.node_mut() {
                    match node.direction {
                        SplitDirection::Horizontal => {
                            left += node.first.cols as usize;
                            pixel_left += node.first.pixel_width;
                        }
                        SplitDirection::Vertical => {
                            top += node.first.rows as usize;
                            pixel_top += node.first.pixel_height;
                        }
                    }

                    dividers.push(PositionedSplit {
                        index,
                        direction: node.direction,
                        left,
                        pixel_left,
                        top,
                        pixel_top,
                        size: if node.direction == SplitDirection::Horizontal {
                            node.height() as usize
                        } else {
                            node.width() as usize
                        },
                        pixel_size: if node.direction == SplitDirection::Horizontal {
                            node.size().pixel_height
                        } else {
                            node.size().pixel_width
                        },
                        divider_pixel_width: node
                            .divider_pixel_width()
                            .max(cell_dimensions.pixel_width),
                        divider_pixel_height: node
                            .divider_pixel_height()
                            .max(cell_dimensions.pixel_height),
                    })
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        dividers
    }

    fn get_size(&self) -> TerminalSize {
        self.size
    }

    fn resize(&mut self, size: TerminalSize) {
        self.resize_impl(size, false);
    }

    fn resize_preserving_split(&mut self, size: TerminalSize) {
        self.resize_impl(size, true);
    }

    fn resize_impl(&mut self, size: TerminalSize, preserve_split: bool) {
        if size.rows == 0 || size.cols == 0 {
            // Ignore "impossible" resize requests
            return;
        }

        if let Some(zoomed) = &self.zoomed {
            self.size = size;
            zoomed.resize(size).ok();
        } else {
            let dims = cell_dimensions(&size);
            let (min_x, min_y) = compute_min_size(self.pane.as_mut().unwrap());
            // Constrain the new size to the minimum possible dimensions
            let cols = size.cols.max(min_x);
            let rows = size.rows.max(min_y);
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols * dims.pixel_width,
                pixel_height: rows * dims.pixel_height,
                dpi: dims.dpi,
            };

            // Update the split nodes with adjusted sizes
            resize_tree_to_size(self.pane.as_mut().unwrap(), &size, &dims);

            self.size = size;

            // And then resize the individual panes to match
            apply_sizes_from_splits_impl(self.pane.as_mut().unwrap(), &size, preserve_split, false);
        }

        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn apply_pane_size(&mut self, pane_size: TerminalSize, cursor: &mut Cursor) {
        let cell_width = pane_size
            .pixel_width
            .checked_div(pane_size.cols)
            .unwrap_or(1);
        let cell_height = pane_size
            .pixel_height
            .checked_div(pane_size.rows)
            .unwrap_or(1);
        if let Ok(Some(node)) = cursor.node_mut() {
            if node.direction == SplitDirection::Horizontal {
                let (first_cols, second_cols) = split_child_lengths(
                    pane_size.cols,
                    node.preferred_first_axis_size(),
                    node.preferred_second_axis_size(),
                    1,
                    1,
                );

                node.first = terminal_size_with_pixels(
                    pane_size.rows,
                    first_cols,
                    pane_size.pixel_height,
                    first_cols * cell_width,
                    pane_size.dpi,
                );
                node.second = terminal_size_with_pixels(
                    pane_size.rows,
                    second_cols,
                    pane_size.pixel_height,
                    second_cols * cell_width,
                    pane_size.dpi,
                );
                node.divider_pixel_width = cell_width;
                node.divider_pixel_height = cell_height;
            } else {
                let (first_rows, second_rows) = split_child_lengths(
                    pane_size.rows,
                    node.preferred_first_axis_size(),
                    node.preferred_second_axis_size(),
                    1,
                    1,
                );

                node.first = terminal_size_with_pixels(
                    first_rows,
                    pane_size.cols,
                    first_rows * cell_height,
                    pane_size.pixel_width,
                    pane_size.dpi,
                );
                node.second = terminal_size_with_pixels(
                    second_rows,
                    pane_size.cols,
                    second_rows * cell_height,
                    pane_size.pixel_width,
                    pane_size.dpi,
                );
                node.divider_pixel_width = cell_width;
                node.divider_pixel_height = cell_height;
            }
        }
    }

    fn rebuild_splits_sizes_from_contained_panes(&mut self, notify: bool) {
        if self.zoomed.is_some() {
            return;
        }

        fn compute_size(node: &mut Tree) -> Option<TerminalSize> {
            match node {
                Tree::Empty => None,
                Tree::Leaf(pane) => {
                    let dims = pane.get_dimensions();
                    let size = TerminalSize {
                        cols: dims.cols,
                        rows: dims.viewport_rows,
                        pixel_height: dims.pixel_height,
                        pixel_width: dims.pixel_width,
                        dpi: dims.dpi,
                    };
                    Some(size)
                }
                Tree::Node { left, right, data } => {
                    if let Some(data) = data {
                        if let Some(first) = compute_size(left) {
                            data.first = first;
                        }
                        if let Some(second) = compute_size(right) {
                            data.second = second;
                        }
                        Some(data.size())
                    } else {
                        None
                    }
                }
            }
        }

        if let Some(root) = self.pane.as_mut() {
            if let Some(size) = compute_size(root) {
                self.size = size;
            }
        }
        if notify {
            Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        }
    }

    fn resize_split_by(&mut self, split_index: usize, delta: isize) {
        self.resize_split_by_impl(split_index, delta, false)
    }

    fn resize_split_by_preserving_split(&mut self, split_index: usize, delta: isize) {
        self.resize_split_by_impl(split_index, delta, true)
    }

    fn resize_split_by_impl(&mut self, split_index: usize, delta: isize, preserve_split: bool) {
        if self.zoomed.is_some() {
            return;
        }

        let cell_dimensions = self.cell_dimensions();
        let mut index = 0;
        if let Some(root) = self.pane.as_mut() {
            if !Self::adjust_split_by_index(root, split_index, &mut index, delta, &cell_dimensions)
            {
                return;
            }
            if preserve_split {
                apply_sizes_from_splits_preserving_split_for_split_drag(root, &self.size);
            } else {
                apply_sizes_from_splits(root, &self.size);
            }
            Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        }
    }

    fn adjust_split_by_index(
        tree: &mut Tree,
        split_index: usize,
        index: &mut usize,
        delta: isize,
        cell_dimensions: &TerminalSize,
    ) -> bool {
        match tree {
            Tree::Empty | Tree::Leaf(_) => false,
            Tree::Node { data: None, .. } => false,
            Tree::Node {
                left,
                right,
                data: Some(node),
            } => {
                let is_target = *index == split_index;
                *index += 1;
                if !is_target {
                    return Self::adjust_split_by_index(
                        &mut *left,
                        split_index,
                        index,
                        delta,
                        cell_dimensions,
                    ) || Self::adjust_split_by_index(
                        &mut *right,
                        split_index,
                        index,
                        delta,
                        cell_dimensions,
                    );
                }

                match node.direction {
                    SplitDirection::Horizontal => {
                        let width = node.width();
                        let pixel_width = node.size().pixel_width;
                        let (first_min, _) = compute_min_size(&mut *left);
                        let (second_min, _) = compute_min_size(&mut *right);
                        let old_first = node.first;
                        let old_second = node.second;

                        let requested_cols = (node.first.cols as isize).saturating_add(delta);
                        let min_cols = first_min as isize;
                        let max_cols = (width as isize)
                            .saturating_sub(second_min as isize)
                            .saturating_sub(1);
                        let cols = requested_cols.max(min_cols).min(max_cols);
                        node.first.cols = cols as usize;

                        node.second.cols = width.saturating_sub(node.first.cols.saturating_add(1));
                        if requested_cols == cols {
                            node.set_preferred_from_current();
                        }

                        let (first_pixel_width, second_pixel_width) = split_child_pixels(
                            pixel_width,
                            node.first.cols,
                            node.divider_pixel_width().max(cell_dimensions.pixel_width),
                            cell_dimensions.pixel_width,
                        );
                        node.first.pixel_width = first_pixel_width;
                        node.second.pixel_width = second_pixel_width;

                        if node.first == old_first && node.second == old_second {
                            return false;
                        }

                        resize_tree_to_size(&mut *left, &node.first, cell_dimensions);
                        resize_tree_to_size(&mut *right, &node.second, cell_dimensions);
                    }
                    SplitDirection::Vertical => {
                        let height = node.height();
                        let pixel_height = node.size().pixel_height;
                        let (_, first_min) = compute_min_size(&mut *left);
                        let (_, second_min) = compute_min_size(&mut *right);
                        let old_first = node.first;
                        let old_second = node.second;

                        let requested_rows = (node.first.rows as isize).saturating_add(delta);
                        let min_rows = first_min as isize;
                        let max_rows = (height as isize)
                            .saturating_sub(second_min as isize)
                            .saturating_sub(1);
                        let rows = requested_rows.max(min_rows).min(max_rows);
                        node.first.rows = rows as usize;

                        node.second.rows = height.saturating_sub(node.first.rows.saturating_add(1));
                        if requested_rows == rows {
                            node.set_preferred_from_current();
                        }

                        let (first_pixel_height, second_pixel_height) = split_child_pixels(
                            pixel_height,
                            node.first.rows,
                            node.divider_pixel_height()
                                .max(cell_dimensions.pixel_height),
                            cell_dimensions.pixel_height,
                        );
                        node.first.pixel_height = first_pixel_height;
                        node.second.pixel_height = second_pixel_height;

                        if node.first == old_first && node.second == old_second {
                            return false;
                        }

                        resize_tree_to_size(&mut *left, &node.first, cell_dimensions);
                        resize_tree_to_size(&mut *right, &node.second, cell_dimensions);
                    }
                }
                true
            }
        }
    }

    fn adjust_pane_size(&mut self, direction: PaneDirection, amount: usize) {
        if self.zoomed.is_some() {
            return;
        }

        // We are on the active leaf.
        // Now we go up until we find the parent node that is
        // aligned with the desired direction.
        let split_direction = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        let delta = match direction {
            PaneDirection::Down | PaneDirection::Right => amount as isize,
            PaneDirection::Up | PaneDirection::Left => -(amount as isize),
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };

        if let Some(split_index) = self.resize_split_index_for_active_pane(split_direction) {
            self.resize_split_by(split_index, delta);
        }
    }

    fn resize_split_index_for_active_pane(
        &mut self,
        split_direction: SplitDirection,
    ) -> Option<usize> {
        fn walk(
            tree: &Tree,
            active_index: usize,
            split_direction: SplitDirection,
            leaf_index: &mut usize,
            split_index: &mut usize,
        ) -> (bool, Option<usize>) {
            match tree {
                Tree::Empty => (false, None),
                Tree::Leaf(_) => {
                    let contains_active = *leaf_index == active_index;
                    *leaf_index += 1;
                    (contains_active, None)
                }
                Tree::Node { left, right, data } => {
                    let current_split_index = *split_index;
                    *split_index += 1;

                    let (left_contains, left_match) = walk(
                        &*left,
                        active_index,
                        split_direction,
                        leaf_index,
                        split_index,
                    );
                    let (right_contains, right_match) = walk(
                        &*right,
                        active_index,
                        split_direction,
                        leaf_index,
                        split_index,
                    );

                    let contains_active = left_contains || right_contains;
                    if !contains_active {
                        return (false, None);
                    }
                    if let Some(split_index) = left_match.or(right_match) {
                        return (true, Some(split_index));
                    }
                    if data
                        .as_ref()
                        .map(|data| data.direction == split_direction)
                        .unwrap_or(false)
                    {
                        return (true, Some(current_split_index));
                    }
                    (true, None)
                }
            }
        }

        let mut leaf_index = 0;
        let mut split_index = 0;
        self.pane.as_ref().and_then(|pane| {
            walk(
                pane,
                self.active,
                split_direction,
                &mut leaf_index,
                &mut split_index,
            )
            .1
        })
    }

    fn activate_pane_direction(&mut self, direction: PaneDirection) {
        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }
        if let Some(panel_idx) = self.get_pane_direction(direction, false) {
            self.set_active_idx(panel_idx);
        }
        let mux = Mux::get();
        if let Some(window_id) = mux.window_containing_tab(self.id) {
            mux.notify(MuxNotification::WindowInvalidated(window_id));
        }
    }

    fn get_pane_direction(&mut self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        let panes = if ignore_zoom {
            self.iter_panes_ignoring_zoom()
        } else {
            self.iter_panes()
        };

        let active = match panes.iter().find(|pane| pane.is_active) {
            Some(p) => p,
            None => {
                // No active pane somehow...
                return Some(0);
            }
        };

        if matches!(direction, PaneDirection::Next | PaneDirection::Prev) {
            let max_pane_id = panes.iter().map(|p| p.index).max().unwrap_or(active.index);

            return Some(if direction == PaneDirection::Next {
                if active.index == max_pane_id {
                    0
                } else {
                    active.index + 1
                }
            } else {
                if active.index == 0 {
                    max_pane_id
                } else {
                    active.index - 1
                }
            });
        }

        let mut best = None;

        let recency = &self.recency;
        let cell_dims = self.cell_dimensions();

        fn edge_intersects(
            active_start: usize,
            active_size: usize,
            current_start: usize,
            current_size: usize,
        ) -> bool {
            intersects_range(
                &(active_start..active_start + active_size),
                &(current_start..current_start + current_size),
            )
        }

        for pane in &panes {
            let score = match direction {
                PaneDirection::Right => {
                    if pane.pixel_left
                        == active.pixel_left + active.pixel_width + cell_dims.pixel_width
                        && edge_intersects(
                            active.pixel_top,
                            active.pixel_height,
                            pane.pixel_top,
                            pane.pixel_height,
                        )
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Left => {
                    if pane.pixel_left + pane.pixel_width + cell_dims.pixel_width
                        == active.pixel_left
                        && edge_intersects(
                            active.pixel_top,
                            active.pixel_height,
                            pane.pixel_top,
                            pane.pixel_height,
                        )
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Up => {
                    if pane.pixel_top + pane.pixel_height + cell_dims.pixel_height
                        == active.pixel_top
                        && edge_intersects(
                            active.pixel_left,
                            active.pixel_width,
                            pane.pixel_left,
                            pane.pixel_width,
                        )
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Down => {
                    if active.pixel_top + active.pixel_height + cell_dims.pixel_height
                        == pane.pixel_top
                        && edge_intersects(
                            active.pixel_left,
                            active.pixel_width,
                            pane.pixel_left,
                            pane.pixel_width,
                        )
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Next | PaneDirection::Prev => unreachable!(),
            };

            if score > 0 {
                let target = match best.take() {
                    Some((best_score, best_pane)) if best_score > score => (best_score, best_pane),
                    _ => (score, pane),
                };
                best.replace(target);
            }
        }

        if let Some((_, target)) = best.take() {
            return Some(target.index);
        }
        None
    }

    fn prune_dead_panes(&mut self, preserve_split: bool) -> bool {
        let mux = Mux::get();
        !self
            .remove_pane_if(
                |_, pane| {
                    // If the pane is no longer known to the mux, then its liveness
                    // state isn't guaranteed to be monitored or updated, so let's
                    // consider the pane effectively dead if it isn't in the mux.
                    // <https://github.com/wezterm/wezterm/issues/4030>
                    let in_mux = mux.get_pane(pane.pane_id()).is_some();
                    let dead = pane.is_dead();
                    log::trace!(
                        "prune_dead_panes: pane_id={} dead={} in_mux={}",
                        pane.pane_id(),
                        dead,
                        in_mux
                    );
                    dead || !in_mux
                },
                true,
                preserve_split,
            )
            .is_empty()
    }

    fn kill_pane(&mut self, pane_id: PaneId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.pane_id() == pane_id, true, false)
            .is_empty()
    }

    fn kill_panes_in_domain(&mut self, domain: DomainId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.domain_id() == domain, true, false)
            .is_empty()
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let panes = self.remove_pane_if(|_, pane| pane.pane_id() == pane_id, false, false);
        for pane in panes {
            return Some(pane);
        }
        None
    }

    fn remove_pane_if<F>(&mut self, f: F, kill: bool, preserve_split: bool) -> Vec<Arc<dyn Pane>>
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        fn resize_pane(pane: &Arc<dyn Pane>, size: TerminalSize, preserve_split: bool) {
            if preserve_split {
                pane.resize_preserving_split(size).ok();
            } else {
                pane.resize(size).ok();
            }
        }

        let mut dead_panes = vec![];
        let zoomed_pane = self.zoomed.as_ref().map(|p| p.pane_id());

        {
            let root_size = self.size;
            let mut cursor = self.pane.take().unwrap().cursor();
            let mut pane_index = 0;
            let mut removed_indices = vec![];
            let cell_dims = self.cell_dimensions();

            loop {
                // Figure out the available size by looking at our immediate parent node.
                // If we are the root, look at the tab size
                let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                    if branch == PathBranch::IsRight {
                        parent.second
                    } else {
                        parent.first
                    }
                } else {
                    root_size
                };

                if cursor.is_leaf() {
                    let pane = Arc::clone(cursor.leaf_mut().unwrap());
                    if f(pane_index, &pane) {
                        removed_indices.push(pane_index);
                        if Some(pane.pane_id()) == zoomed_pane {
                            // If we removed the zoomed pane, un-zoom our state!
                            self.zoomed.take();
                        }
                        let parent;
                        match cursor.unsplit_leaf() {
                            Ok((c, dead, p)) => {
                                dead_panes.push(dead);
                                parent = p.unwrap();
                                cursor = c;
                            }
                            Err(c) => {
                                // We might be the root, for example
                                if c.is_top() && c.is_leaf() {
                                    self.pane.replace(Tree::Empty);
                                    dead_panes.push(pane);
                                } else {
                                    self.pane.replace(c.tree());
                                }
                                break;
                            }
                        };

                        // Now we need to increase the size of the current node
                        // and propagate the revised size to its children.
                        let size = TerminalSize {
                            rows: parent.height(),
                            cols: parent.width(),
                            pixel_width: cell_dims.pixel_width * parent.width(),
                            pixel_height: cell_dims.pixel_height * parent.height(),
                            dpi: cell_dims.dpi,
                        };

                        if let Some(unsplit) = cursor.leaf_mut() {
                            resize_pane(unsplit, size, preserve_split);
                        } else {
                            self.apply_pane_size(size, &mut cursor);
                        }
                    } else if !dead_panes.is_empty() {
                        // Apply our revised size to the tty
                        resize_pane(&pane, pane_size, preserve_split);
                    }

                    pane_index += 1;
                } else if !dead_panes.is_empty() {
                    self.apply_pane_size(pane_size, &mut cursor);
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        self.pane.replace(c.tree());
                        break;
                    }
                }
            }

            // Figure out which pane should now be active.
            // If panes earlier than the active pane were closed, then we
            // need to shift the active pane down
            let active_idx = self.active;
            removed_indices.retain(|&idx| idx <= active_idx);
            self.active = active_idx.saturating_sub(removed_indices.len());
        }

        if !dead_panes.is_empty() {
            Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        }

        if !dead_panes.is_empty() && kill {
            let to_kill: Vec<_> = dead_panes.iter().map(|p| p.pane_id()).collect();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                for pane_id in to_kill.into_iter() {
                    mux.remove_pane(pane_id);
                }
            })
            .detach();
        }
        dead_panes
    }

    fn can_close_without_prompting(&mut self, reason: CloseReason) -> bool {
        let panes = self.iter_panes_ignoring_zoom();
        for pos in &panes {
            if !pos.pane.can_close_without_prompting(reason) {
                return false;
            }
        }
        true
    }

    fn is_dead(&mut self) -> bool {
        // Make sure we account for all panes, so that we don't
        // kill the whole tab if the zoomed pane is dead!
        let panes = self.iter_panes_ignoring_zoom();
        let mut dead_count = 0;
        for pos in &panes {
            if pos.pane.is_dead() {
                dead_count += 1;
            }
        }
        dead_count == panes.len()
    }

    fn get_active_pane(&mut self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = self.zoomed.as_ref() {
            return Some(Arc::clone(zoomed));
        }

        self.iter_panes_ignoring_zoom()
            .iter()
            .nth(self.active)
            .map(|p| Arc::clone(&p.pane))
    }

    fn get_active_idx(&self) -> usize {
        self.active
    }

    fn set_active_pane(&mut self, pane: &Arc<dyn Pane>, notify: bool) {
        let prior = self.get_active_pane();

        if is_pane(pane, &prior.as_ref()) {
            return;
        }

        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }

        if let Some(item) = self
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane.pane_id())
        {
            self.active = item.index;
            self.recency.tag(item.index);
            self.advise_focus_change(prior, notify);
        }
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>, notify: bool) {
        let mux = Mux::get();
        let current = self.get_active_pane();
        match (prior, current) {
            (Some(prior), Some(current)) if prior.pane_id() != current.pane_id() => {
                prior.focus_changed(false);
                current.focus_changed(true);
                if notify {
                    mux.notify(MuxNotification::PaneFocused(current.pane_id()));
                }
            }
            (None, Some(current)) => {
                current.focus_changed(true);
                if notify {
                    mux.notify(MuxNotification::PaneFocused(current.pane_id()));
                }
            }
            (Some(prior), None) => {
                prior.focus_changed(false);
            }
            (Some(_), Some(_)) | (None, None) => {
                // no change
            }
        }
    }

    fn set_active_idx(&mut self, pane_index: usize) {
        let prior = self.get_active_pane();
        self.active = pane_index;
        self.recency.tag(pane_index);
        self.advise_focus_change(prior, true);
    }

    fn assign_pane(&mut self, pane: &Arc<dyn Pane>) {
        match Tree::new().cursor().assign_top(Arc::clone(pane)) {
            Ok(c) => self.pane = Some(c.tree()),
            Err(_) => panic!("tried to assign root pane to non-empty tree"),
        }
    }

    #[allow(dead_code)] // Used by Tab::replace_pane_tree.
    fn validate_pane_tree(tree: &Tree, active_pane_id: PaneId) -> anyhow::Result<Vec<PaneId>> {
        fn collect_panes(
            tree: &Tree,
            panes: &mut Vec<PaneId>,
            seen: &mut std::collections::HashSet<PaneId>,
        ) -> anyhow::Result<()> {
            match tree {
                Tree::Empty => anyhow::bail!("replacement pane tree is empty"),
                Tree::Leaf(pane) => {
                    let pane_id = pane.pane_id();
                    if !seen.insert(pane_id) {
                        anyhow::bail!("replacement pane tree contains duplicate pane {pane_id}");
                    }
                    panes.push(pane_id);
                }
                Tree::Node {
                    left, right, data, ..
                } => {
                    if data.is_none() {
                        anyhow::bail!("replacement pane tree contains an unlabeled split");
                    }
                    collect_panes(left, panes, seen)?;
                    collect_panes(right, panes, seen)?;
                }
            }
            Ok(())
        }

        let mut pane_ids = vec![];
        collect_panes(tree, &mut pane_ids, &mut Default::default())?;
        if !pane_ids.contains(&active_pane_id) {
            anyhow::bail!("active pane {active_pane_id} is absent from replacement tree");
        }
        Ok(pane_ids)
    }

    fn replace_pane_tree(
        &mut self,
        tree: Tree,
        active_pane_id: PaneId,
        notify: bool,
    ) -> anyhow::Result<()> {
        let pane_ids = Self::validate_pane_tree(&tree, active_pane_id)?;
        let active = pane_ids
            .iter()
            .position(|pane_id| *pane_id == active_pane_id)
            .expect("validated active pane must be present");

        let prior = self.get_active_pane();
        let zoomed_pane_id = self.zoomed.as_ref().map(|pane| pane.pane_id());
        self.pane = Some(tree);
        self.active = active;
        self.recency.tag(active);
        if zoomed_pane_id.is_some_and(|pane_id| !pane_ids.contains(&pane_id)) {
            self.zoomed = None;
        }
        apply_sizes_from_splits_preserving_split(self.pane.as_mut().unwrap(), &self.size);
        if let Some(mux) = Mux::try_get() {
            self.advise_focus_change(prior, notify);
            if notify {
                mux.notify(MuxNotification::TabResized(self.id));
            }
        }
        Ok(())
    }

    fn cell_dimensions(&self) -> TerminalSize {
        cell_dimensions(&self.size)
    }

    fn swap_active_with_index(&mut self, pane_index: usize, keep_focus: bool) -> Option<()> {
        let active_idx = self.get_active_idx();
        let mut pane = self.get_active_pane()?;
        log::trace!(
            "swap_active_with_index: pane_index {} active {}",
            pane_index,
            active_idx
        );

        {
            let mut cursor = self.pane.take().unwrap().cursor();

            // locate the requested index
            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    log::trace!("didn't find pane {pane_index}");
                    self.pane.replace(c.tree());
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());

            // re-position to the root
            cursor = cursor.tree().cursor();

            // and now go and update the active idx
            match cursor.go_to_nth_leaf(active_idx) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    log::trace!("didn't find active {active_idx}");
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());
            self.pane.replace(cursor.tree());

            // Advise the panes of their new sizes
            let size = self.size;
            apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
        }

        // And update focus
        if keep_focus {
            self.set_active_idx(pane_index);
        } else {
            self.advise_focus_change(Some(pane), true);
        }
        None
    }

    fn reposition_pane(
        &mut self,
        pane_id: PaneId,
        target_pane_id: PaneId,
        request: SplitRequest,
    ) -> anyhow::Result<()> {
        if pane_id == target_pane_id {
            return Ok(());
        }
        if self.zoomed.is_some() {
            anyhow::bail!("cannot reposition panes while zoomed");
        }

        let panes = self.iter_panes_ignoring_zoom();
        panes
            .iter()
            .position(|pos| pos.pane.pane_id() == pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} is not in this tab"))?;
        let target_index = panes
            .iter()
            .position(|pos| pos.pane.pane_id() == target_pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {target_pane_id} is not in this tab"))?;

        // Check that the target can be split before changing the tree.
        self.compute_split_size(target_index, request)
            .ok_or_else(|| anyhow::anyhow!("target pane is too small to split"))?;
        let pane = self
            .remove_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("failed to detach pane {pane_id}"))?;
        let target_index = self
            .iter_panes_ignoring_zoom()
            .iter()
            .position(|pos| pos.pane.pane_id() == target_pane_id)
            .ok_or_else(|| anyhow::anyhow!("target pane disappeared while repositioning"))?;
        let new_index = self.split_and_insert(target_index, request, pane, false)?;
        self.active = new_index;
        self.recency.tag(new_index);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
        Ok(())
    }

    fn compute_split_size(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        let cell_dims = self.cell_dimensions();

        if request.top_level {
            let size = self.size;

            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimensions(size.cols, request)?,
                    (size.rows, size.rows),
                ),
                SplitDirection::Vertical => (
                    (size.cols, size.cols),
                    split_dimensions(size.rows, request)?,
                ),
            };

            return Some(split_size_from_parent(
                size,
                request.direction,
                height1,
                height2,
                width1,
                width2,
                &cell_dims,
            ));
        }

        // Ensure that we're not zoomed, otherwise we'll end up in
        // a bogus split state (https://github.com/wezterm/wezterm/issues/723)
        self.set_zoomed(false);

        self.iter_panes().iter().nth(pane_index).and_then(|pos| {
            let layout_width = if cell_dims.pixel_width == 0 {
                pos.width
            } else {
                (pos.pixel_width / cell_dims.pixel_width).max(1)
            };
            let layout_height = if cell_dims.pixel_height == 0 {
                pos.height
            } else {
                (pos.pixel_height / cell_dims.pixel_height).max(1)
            };
            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimensions(layout_width, request)?,
                    (layout_height, layout_height),
                ),
                SplitDirection::Vertical => (
                    (layout_width, layout_width),
                    split_dimensions(layout_height, request)?,
                ),
            };

            Some(split_size_from_parent(
                TerminalSize {
                    rows: layout_height,
                    cols: layout_width,
                    pixel_height: pos.pixel_height,
                    pixel_width: pos.pixel_width,
                    dpi: cell_dims.dpi,
                },
                request.direction,
                height1,
                height2,
                width1,
                width2,
                &cell_dims,
            ))
        })
    }

    fn split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
        preserve_split: bool,
    ) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

        {
            let split_info = self
                .compute_split_size(pane_index, request)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid pane_index {}; cannot split!", pane_index)
                })?;

            let tab_size = self.size;
            if split_info.first.rows == 0
                || split_info.first.cols == 0
                || split_info.second.rows == 0
                || split_info.second.cols == 0
                || split_info
                    .top_of_second()
                    .saturating_add(split_info.second.rows)
                    > tab_size.rows
                || split_info
                    .left_of_second()
                    .saturating_add(split_info.second.cols)
                    > tab_size.cols
            {
                log::error!(
                    "No space for split!!! {:#?} height={} width={} top_of_second={} left_of_second={} tab_size={:?}",
                    split_info,
                    split_info.height(),
                    split_info.width(),
                    split_info.top_of_second(),
                    split_info.left_of_second(),
                    tab_size
                );
                anyhow::bail!("No space for split!");
            }

            let needs_resize = if request.top_level {
                self.pane.as_ref().unwrap().num_leaves() > 1
            } else {
                false
            };

            if needs_resize {
                // Pre-emptively resize the tab contents down to
                // match the target size; it's easier to reuse
                // existing resize logic that way
                if request.target_is_second {
                    self.resize_impl(split_info.first.clone(), preserve_split);
                } else {
                    self.resize_impl(split_info.second.clone(), preserve_split);
                }
            }

            let mut cursor = self.pane.take().unwrap().cursor();

            if request.top_level && !cursor.is_leaf() {
                let result = if request.target_is_second {
                    cursor.split_node_and_insert_right(Arc::clone(&pane))
                } else {
                    cursor.split_node_and_insert_left(Arc::clone(&pane))
                };
                cursor = match result {
                    Ok(c) => {
                        cursor = match c.assign_node(Some(split_info)) {
                            Err(c) | Ok(c) => c,
                        };

                        self.pane.replace(cursor.tree());

                        let pane_index = if request.target_is_second {
                            self.pane.as_ref().unwrap().num_leaves().saturating_sub(1)
                        } else {
                            0
                        };

                        self.active = pane_index;
                        self.recency.tag(pane_index);
                        return Ok(pane_index);
                    }
                    Err(cursor) => cursor,
                };
            }

            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            let existing_pane = Arc::clone(cursor.leaf_mut().unwrap());

            let (pane1, pane2) = if request.target_is_second {
                (existing_pane, pane)
            } else {
                (pane, existing_pane)
            };

            if preserve_split {
                pane1.resize_preserving_split(split_info.first)?;
                pane2.resize_preserving_split(split_info.second.clone())?;
            } else {
                pane1.resize(split_info.first)?;
                pane2.resize(split_info.second.clone())?;
            }

            *cursor.leaf_mut().unwrap() = pane1;

            match cursor.split_leaf_and_insert_right(pane2) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            // cursor now points to the newly created split node;
            // we need to populate its split information
            match cursor.assign_node(Some(split_info)) {
                Err(c) | Ok(c) => self.pane.replace(c.tree()),
            };

            if request.target_is_second {
                self.active = pane_index + 1;
                self.recency.tag(pane_index + 1);
            }
        }

        log::debug!("split info after split: {:#?}", self.iter_splits());
        log::debug!("pane info after split: {:#?}", self.iter_panes());
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));

        Ok(if request.target_is_second {
            pane_index + 1
        } else {
            pane_index
        })
    }

    fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.zoomed.clone()
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub enum PaneNode {
    Empty,
    TabRoot {
        root: Box<PaneNode>,
        left_sidebar_hidden: bool,
        right_sidebar_hidden: bool,
    },
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

impl PaneNode {
    pub fn into_tree(self) -> bintree::Tree<PaneEntry, SplitDirectionAndSize> {
        match self {
            PaneNode::Empty => bintree::Tree::Empty,
            PaneNode::TabRoot { root, .. } => (*root).into_tree(),
            PaneNode::Split { left, right, node } => bintree::Tree::Node {
                left: Box::new((*left).into_tree()),
                right: Box::new((*right).into_tree()),
                data: Some(node),
            },
            PaneNode::Leaf(e) => bintree::Tree::Leaf(e),
        }
    }

    pub fn root_size(&self) -> Option<TerminalSize> {
        match self {
            PaneNode::Empty => None,
            PaneNode::TabRoot { root, .. } => root.root_size(),
            PaneNode::Split { node, .. } => Some(node.size()),
            PaneNode::Leaf(entry) => Some(entry.size),
        }
    }

    pub fn window_and_tab_ids(&self) -> Option<(WindowId, TabId)> {
        match self {
            PaneNode::Empty => None,
            PaneNode::TabRoot { root, .. } => root.window_and_tab_ids(),
            PaneNode::Split { left, right, .. } => match left.window_and_tab_ids() {
                Some(res) => Some(res),
                None => right.window_and_tab_ids(),
            },
            PaneNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
        }
    }

    pub fn left_sidebar_hidden(&self) -> bool {
        matches!(
            self,
            PaneNode::TabRoot {
                left_sidebar_hidden: true,
                ..
            }
        )
    }

    pub fn right_sidebar_hidden(&self) -> bool {
        matches!(
            self,
            PaneNode::TabRoot {
                right_sidebar_hidden: true,
                ..
            }
        )
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub top_px: usize,
    pub left_px: usize,
    pub font_scale: Option<f64>,
    pub tty_name: Option<String>,
    #[serde(default)]
    pub tmux_connection_state: Option<TmuxConnectionState>,
}

#[derive(Deserialize, Clone, Copy, Serialize, PartialEq, Eq, Debug)]
pub enum TmuxConnectionState {
    Connecting,
    Syncing,
    Connected,
    Reconnecting,
    Disconnected,
}

#[derive(Deserialize, Clone, Serialize, PartialEq, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct SerdeUrl {
    pub url: Url,
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        Ok(SerdeUrl { url })
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { url }
    }
}

impl Into<Url> for SerdeUrl {
    fn into(self) -> Url {
        self.url
    }
}

impl Into<String> for SerdeUrl {
    fn into(self) -> String {
        self.url.as_str().into()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::*;
    use parking_lot::{MappedMutexGuard, Mutex};
    use rangeset::RangeSet;
    use std::ops::Range;
    use termwiz::surface::SequenceNo;
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
            })
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            unimplemented!();
        }

        fn get_current_seqno(&self) -> SequenceNo {
            unimplemented!();
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
            unimplemented!();
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            unimplemented!();
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            unimplemented!();
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            unimplemented!();
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                ..Default::default()
            }
        }

        fn get_title(&self) -> String {
            unimplemented!()
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            unimplemented!()
        }
        fn domain_id(&self) -> DomainId {
            1
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
    }

    #[test]
    fn remote_tree_sync_preserves_existing_pane_dimensions() {
        let remote_size = TerminalSize {
            rows: 18,
            cols: 36,
            pixel_width: 360,
            pixel_height: 342,
            dpi: 96,
        };
        let font_aware_size = TerminalSize {
            rows: 22,
            cols: 51,
            ..remote_size
        };
        let pane = FakePane::new(1, font_aware_size);
        let tab = Tab::new(&remote_size);
        let pane_for_sync = Arc::clone(&pane);
        let root = PaneNode::Leaf(PaneEntry {
            window_id: 1,
            tab_id: tab.tab_id(),
            pane_id: pane.pane_id(),
            title: String::new(),
            size: remote_size,
            working_dir: None,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: String::new(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            top_px: 0,
            left_px: 0,
            font_scale: Some(0.75),
            tty_name: None,
            tmux_connection_state: None,
        });

        tab.sync_with_pane_tree_preserving_pane_sizes(remote_size, root, move |_| {
            Arc::clone(&pane_for_sync)
        });

        let dimensions = pane.get_dimensions();
        assert_eq!(dimensions.cols, font_aware_size.cols);
        assert_eq!(dimensions.viewport_rows, font_aware_size.rows);
        assert_eq!(dimensions.pixel_width, font_aware_size.pixel_width);
        assert_eq!(dimensions.pixel_height, font_aware_size.pixel_height);
    }

    #[test]
    fn tab_splitting() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let panes = tab.iter_panes();
        assert_eq!(1, panes.len());
        assert_eq!(0, panes[0].index);
        assert_eq!(true, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(80, panes[0].width);
        assert_eq!(24, panes[0].height);

        assert!(tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                }
            )
            .is_none());

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            horz_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 600,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 24,
                    cols: 39,
                    pixel_width: 390,
                    pixel_height: 600,
                    dpi: 96,
                },
                divider_pixel_width: 10,
                divider_pixel_height: 25,
                preferred_first: 39,
                preferred_second: 40,
            }
        );

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            vert_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                second: TerminalSize {
                    rows: 12,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 300,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 11,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 275,
                    dpi: 96,
                },
                divider_pixel_width: 10,
                divider_pixel_height: 25,
                preferred_first: 11,
                preferred_second: 12,
            }
        );

        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(2, horz_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(2, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(24, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(600, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(40, panes[1].left);
        assert_eq!(0, panes[1].top);
        assert_eq!(40, panes[1].width);
        assert_eq!(24, panes[1].height);
        assert_eq!(400, panes[1].pixel_width);
        assert_eq!(600, panes[1].pixel_height);
        assert_eq!(2, panes[1].pane.pane_id());

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    top_level: false,
                    target_is_second: true,
                    size: Default::default(),
                },
                FakePane::new(3, vert_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(11, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(275, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(0, panes[1].left);
        assert_eq!(12, panes[1].top);
        assert_eq!(39, panes[1].width);
        assert_eq!(12, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(300, panes[1].pixel_height);
        assert_eq!(3, panes[1].pane.pane_id());

        assert_eq!(2, panes[2].index);
        assert_eq!(false, panes[2].is_active);
        assert_eq!(40, panes[2].left);
        assert_eq!(0, panes[2].top);
        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
        assert_eq!(2, panes[2].pane.pane_id());

        tab.resize_split_by(1, 1);
        let panes = tab.iter_panes();
        assert_eq!(39, panes[0].width);
        assert_eq!(12, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(300, panes[0].pixel_height);

        assert_eq!(39, panes[1].width);
        assert_eq!(11, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(275, panes[1].pixel_height);

        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
    }

    #[test]
    fn split_size_works_without_pixel_dimensions() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    size: SplitSize::Cells(39),
                    ..Default::default()
                },
            )
            .expect("headless/tmux panes must be splittable without pixel dimensions");
        assert_eq!(split.first.cols + split.second.cols + 1, size.cols);
        assert_eq!(split.first.rows, size.rows);
        assert_eq!(split.second.rows, size.rows);
    }

    #[test]
    fn split_resize_preserves_nested_minimum_size() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, horz_size.second),
        )
        .unwrap();

        let nested_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(3, nested_size.second),
        )
        .unwrap();

        tab.resize_split_by(0, -1000);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].left);
        assert_eq!(1, panes[0].width);
        assert_eq!(2, panes[1].left);
        assert_eq!(1, panes[1].width);
        assert_eq!(4, panes[2].left);
        assert_eq!(76, panes[2].width);
    }

    #[test]
    fn split_resize_restores_nested_size_when_parent_grows() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, horz_size.second),
        )
        .unwrap();

        let nested_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(3, nested_size.second),
        )
        .unwrap();

        tab.resize_split_by(0, -1000);
        tab.resize_split_by(0, 36);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].left);
        assert_eq!(19, panes[0].width);
        assert_eq!(20, panes[1].left);
        assert_eq!(19, panes[1].width);
        assert_eq!(40, panes[2].left);
        assert_eq!(40, panes[2].width);
    }

    #[test]
    fn slow_parent_growth_restores_squeezed_nested_splits() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 1, 3, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 2, 4, SplitDirection::Horizontal).unwrap();

        tab.resize_split_by(0, 1000);
        for _ in 0..30 {
            tab.resize_split_by(0, -1);
        }

        let panes = tab.iter_panes();
        assert_eq!(4, panes.len());

        let nested_widths: Vec<usize> = panes[1..].iter().map(|pane| pane.width).collect();
        assert!(
            nested_widths.iter().all(|width| *width > 4),
            "nested pane widths should recover from minimum sizing: {:?}",
            nested_widths,
        );

        assert_eq!(
            33,
            nested_widths.iter().sum::<usize>(),
            "nested pane widths should retain the restored parent space: {:?}",
            nested_widths,
        );
    }

    #[test]
    fn rebuild_does_not_make_squeezed_nested_sizes_preferred() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 1, 3, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 2, 4, SplitDirection::Horizontal).unwrap();

        tab.resize_split_by(0, 1000);
        tab.rebuild_splits_sizes_from_contained_panes();

        for _ in 0..30 {
            tab.resize_split_by(0, -1);
            tab.rebuild_splits_sizes_from_contained_panes();
        }

        let panes = tab.iter_panes();
        assert_eq!(4, panes.len());

        let nested_widths: Vec<usize> = panes[1..].iter().map(|pane| pane.width).collect();
        assert!(
            nested_widths.iter().all(|width| *width > 4),
            "nested pane widths should recover after server rebuilds: {:?}",
            nested_widths,
        );
    }

    #[test]
    fn clamped_drag_does_not_make_minimum_size_preferred() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 1, 3, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 2, 4, SplitDirection::Horizontal).unwrap();

        tab.resize_split_by(0, 1000);
        tab.rebuild_splits_sizes_from_contained_panes();

        for _ in 0..30 {
            tab.resize_split_by(0, -1);
            tab.rebuild_splits_sizes_from_contained_panes();
        }

        tab.resize_split_by(1, -1000);
        tab.rebuild_splits_sizes_from_contained_panes();

        for _ in 0..16 {
            tab.resize_split_by(1, 1);
            tab.rebuild_splits_sizes_from_contained_panes();
        }

        let panes = tab.iter_panes();
        assert_eq!(4, panes.len());

        let nested_widths: Vec<usize> = panes[1..3].iter().map(|pane| pane.width).collect();
        assert!(
            nested_widths.iter().all(|width| *width > 4),
            "clamped nested drag should not pin panes at minimum: {:?}",
            nested_widths,
        );
    }

    #[test]
    fn left_sidebar_toggle_preserves_widget_subtree_and_expands_main_area() {
        let size = TerminalSize {
            rows: 24,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 1, 3, SplitDirection::Vertical).unwrap();

        assert_eq!(
            vec![1, 2, 3],
            tab.iter_panes()
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>()
        );

        tab.set_left_sidebar_hidden(true).unwrap();
        assert!(tab.left_sidebar_hidden());
        let hidden = tab.iter_panes();
        assert_eq!(
            vec![2, 3],
            hidden
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>()
        );
        assert!(hidden.iter().all(|pane| pane.left == 0));
        assert!(
            hidden.iter().all(|pane| pane.width == size.cols),
            "hidden main pane widths: {:?}",
            hidden.iter().map(|pane| pane.width).collect::<Vec<_>>()
        );

        tab.set_left_sidebar_hidden(false).unwrap();
        assert!(!tab.left_sidebar_hidden());
        assert_eq!(
            vec![1, 2, 3],
            tab.iter_panes()
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn left_sidebar_requires_horizontal_root_split() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        split_pane(&tab, 0, 2, SplitDirection::Vertical).unwrap();

        assert!(tab.set_left_sidebar_hidden(true).is_err());
        assert_eq!(2, tab.iter_panes().len());
    }

    #[test]
    fn right_sidebar_toggle_preserves_widget_subtree_and_expands_main_area() {
        let size = TerminalSize {
            rows: 24,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 0, 3, SplitDirection::Vertical).unwrap();

        assert_eq!(
            vec![1, 3, 2],
            tab.iter_panes()
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>()
        );

        tab.set_right_sidebar_hidden(true).unwrap();
        assert!(tab.right_sidebar_hidden());
        let hidden = tab.iter_panes();
        assert_eq!(
            vec![1, 3],
            hidden
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>()
        );
        assert!(hidden.iter().all(|pane| pane.left == 0));
        assert!(hidden.iter().all(|pane| pane.width == size.cols));

        tab.set_right_sidebar_hidden(false).unwrap();
        assert!(!tab.right_sidebar_hidden());
        assert_eq!(
            vec![1, 3, 2],
            tab.iter_panes()
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn right_sidebar_toggle_hides_only_rightmost_column_after_repeated_splits() {
        let size = TerminalSize {
            rows: 24,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };

        for pane_count in 3..=5 {
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new(1, size));
            for pane_index in 0..pane_count - 1 {
                split_pane(&tab, pane_index, pane_index + 2, SplitDirection::Horizontal).unwrap();
            }

            let all_panes = (1..=pane_count).collect::<Vec<_>>();
            assert_eq!(
                all_panes,
                tab.iter_panes()
                    .iter()
                    .map(|pane| pane.pane.pane_id())
                    .collect::<Vec<_>>()
            );

            tab.set_right_sidebar_hidden(true).unwrap();
            assert_eq!(
                (1..pane_count).collect::<Vec<_>>(),
                tab.iter_panes()
                    .iter()
                    .map(|pane| pane.pane.pane_id())
                    .collect::<Vec<_>>(),
                "only the rightmost pane should be hidden for {pane_count} panes"
            );

            tab.set_right_sidebar_hidden(false).unwrap();
            assert_eq!(
                all_panes,
                tab.iter_panes()
                    .iter()
                    .map(|pane| pane.pane.pane_id())
                    .collect::<Vec<_>>(),
                "the original pane order should be restored for {pane_count} panes"
            );
        }
    }

    fn split_pane(
        tab: &Tab,
        pane_index: usize,
        id: PaneId,
        direction: SplitDirection,
    ) -> anyhow::Result<()> {
        let request = SplitRequest {
            direction,
            ..Default::default()
        };
        let split_size = tab.compute_split_size(pane_index, request).unwrap();
        tab.split_and_insert(pane_index, request, FakePane::new(id, split_size.second))?;
        Ok(())
    }

    #[test]
    fn reposition_pane_moves_leaf_beside_target() -> anyhow::Result<()> {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        split_pane(&tab, 0, 2, SplitDirection::Horizontal)?;
        split_pane(&tab, 1, 3, SplitDirection::Vertical)?;

        tab.reposition_pane(
            3,
            1,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: false,
                top_level: false,
                size: SplitSize::Percent(50),
            },
        )?;

        let panes = tab.iter_panes();
        assert_eq!(
            panes
                .iter()
                .map(|pos| pos.pane.pane_id())
                .collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
        assert_eq!(
            panes
                .iter()
                .find(|pos| pos.is_active)
                .unwrap()
                .pane
                .pane_id(),
            3
        );
        assert_no_pixel_overlap(&panes);
        Ok(())
    }

    #[test]
    fn replace_pane_tree_is_atomic_and_preserves_pane_objects() -> anyhow::Result<()> {
        let size = TerminalSize {
            rows: 24,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };
        let first = FakePane::new(1, size);
        let second = FakePane::new(2, size);
        let tab = Tab::new(&size);
        tab.assign_pane(&first);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            Arc::clone(&second),
        )?;

        let split = *tab
            .inner
            .lock()
            .pane
            .as_ref()
            .and_then(|tree| match tree {
                Tree::Node { data, .. } => data.as_ref(),
                _ => None,
            })
            .expect("split data");
        let Tree::Node {
            left,
            right,
            data: snapshot_split,
        } = tab.snapshot_pane_tree()
        else {
            panic!("expected split snapshot");
        };
        assert_eq!(snapshot_split, Some(split));
        assert!(matches!(&*left, Tree::Leaf(pane) if Arc::ptr_eq(pane, &first)));
        assert!(matches!(&*right, Tree::Leaf(pane) if Arc::ptr_eq(pane, &second)));
        let replacement = Tree::Node {
            left: Box::new(Tree::Leaf(Arc::clone(&second))),
            right: Box::new(Tree::Leaf(Arc::clone(&first))),
            data: Some(split),
        };
        tab.replace_pane_tree(replacement, 1)?;

        let panes = tab.iter_panes_ignoring_zoom();
        assert_eq!(
            panes
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert!(Arc::ptr_eq(&panes[0].pane, &second));
        assert!(Arc::ptr_eq(&panes[1].pane, &first));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 1);
        Ok(())
    }

    #[test]
    fn replace_pane_tree_rejects_invalid_candidate_without_mutating() -> anyhow::Result<()> {
        let size = TerminalSize {
            rows: 24,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };
        let pane = FakePane::new(1, size);
        let tab = Tab::new(&size);
        tab.assign_pane(&pane);
        let duplicate = Tree::Node {
            left: Box::new(Tree::Leaf(Arc::clone(&pane))),
            right: Box::new(Tree::Leaf(Arc::clone(&pane))),
            data: Some(SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size,
                second: size,
                divider_pixel_width: 0,
                divider_pixel_height: 0,
                preferred_first: 0,
                preferred_second: 0,
            }),
        };
        assert!(tab.replace_pane_tree(duplicate, 1).is_err());
        assert_eq!(
            tab.iter_panes_ignoring_zoom()
                .iter()
                .map(|pane| pane.pane.pane_id())
                .collect::<Vec<_>>(),
            vec![1]
        );
        Ok(())
    }

    fn assert_no_pixel_overlap(panes: &[PositionedPane]) {
        for (idx, a) in panes.iter().enumerate() {
            let a_right = a.pixel_left + a.pixel_width;
            let a_bottom = a.pixel_top + a.pixel_height;

            for b in &panes[idx + 1..] {
                let b_right = b.pixel_left + b.pixel_width;
                let b_bottom = b.pixel_top + b.pixel_height;
                let overlaps_x = a.pixel_left < b_right && b.pixel_left < a_right;
                let overlaps_y = a.pixel_top < b_bottom && b.pixel_top < a_bottom;

                assert!(
                    !(overlaps_x && overlaps_y),
                    "pane {} at {},{} {}x{} overlaps pane {} at {},{} {}x{}",
                    a.pane.pane_id(),
                    a.pixel_left,
                    a.pixel_top,
                    a.pixel_width,
                    a.pixel_height,
                    b.pane.pane_id(),
                    b.pixel_left,
                    b.pixel_top,
                    b.pixel_width,
                    b.pixel_height,
                );
            }
        }
    }

    #[test]
    fn nested_split_resize_does_not_overlap_pixel_rects() {
        let size = TerminalSize {
            rows: 38,
            cols: 140,
            pixel_width: 1400,
            pixel_height: 874,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 0, 3, SplitDirection::Vertical).unwrap();
        split_pane(&tab, 2, 4, SplitDirection::Vertical).unwrap();
        split_pane(&tab, 1, 5, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 3, 6, SplitDirection::Horizontal).unwrap();
        split_pane(&tab, 4, 7, SplitDirection::Vertical).unwrap();
        split_pane(&tab, 6, 8, SplitDirection::Vertical).unwrap();
        split_pane(&tab, 2, 9, SplitDirection::Horizontal).unwrap();

        assert_no_pixel_overlap(&tab.iter_panes());

        for _ in 0..40 {
            tab.resize_split_by(0, 1);
            tab.resize_split_by(1, -1);
            tab.resize_split_by(2, 1);
            tab.resize_split_by(3, -1);
        }

        assert_no_pixel_overlap(&tab.iter_panes());
    }

    #[test]
    fn split_rebuild_preserves_divider_pixels() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        split_pane(&tab, 0, 2, SplitDirection::Horizontal).unwrap();

        let panes = tab.iter_panes();
        assert_eq!(panes[1].pixel_left, 400);

        panes[0]
            .pane
            .resize(TerminalSize {
                rows: 24,
                cols: 48,
                pixel_width: 390,
                pixel_height: 600,
                dpi: 96,
            })
            .unwrap();
        panes[1]
            .pane
            .resize(TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 600,
                dpi: 96,
            })
            .unwrap();

        tab.rebuild_splits_sizes_from_contained_panes_silently();

        let panes = tab.iter_panes();
        assert_eq!(panes[1].pixel_left, 400);
    }

    #[test]
    fn invalid_split_offsets_do_not_panic() {
        let split = SplitDirectionAndSize {
            direction: SplitDirection::Vertical,
            first: TerminalSize {
                rows: usize::MAX,
                cols: 1,
                pixel_width: 10,
                pixel_height: 10,
                dpi: 96,
            },
            second: TerminalSize {
                rows: 1,
                cols: 1,
                pixel_width: 10,
                pixel_height: 10,
                dpi: 96,
            },
            divider_pixel_width: 10,
            divider_pixel_height: 10,
            preferred_first: usize::MAX,
            preferred_second: 1,
        };

        assert_eq!(usize::MAX, split.top_of_second());
        assert_eq!(usize::MAX, split.height());
    }

    fn is_send_and_sync<T: Send + Sync>() -> bool {
        true
    }

    #[test]
    fn tab_is_send_and_sync() {
        assert!(is_send_and_sync::<Tab>());
    }
}

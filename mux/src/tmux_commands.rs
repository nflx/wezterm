use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, PaneId};
use crate::tab::{
    SplitDirection, SplitDirectionAndSize, SplitRequest, SplitSize, Tab, TabId, Tree,
};
use crate::tmux::{AttachState, TmuxDomain, TmuxDomainState, TmuxRemotePane, TmuxTab};
use crate::tmux_pty::{TmuxChild, TmuxPty};
use crate::{Mux, MuxNotification, Pane};
use anyhow::{anyhow, Context};
use parking_lot::{Condvar, Mutex};
use portable_pty::{MasterPty, PtySize};
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::Arc;
use termwiz::escape::csi::{Cursor, CSI};
use termwiz::escape::{Action, OneBased};
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

pub(crate) trait TmuxCommand: Send + Debug {
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;

    fn guarded_response_count(&self) -> usize {
        1
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        anyhow::bail!("tmux command timed out in domain {domain_id}: {self:?}")
    }

    fn resize_pane_id(&self) -> Option<TmuxPaneId> {
        None
    }

    fn is_full_window_snapshot(&self) -> bool {
        false
    }

    fn is_stale_for_killed_pane(&self, _pane_id: TmuxPaneId) -> bool {
        false
    }

    fn select_pane_id(&self) -> Option<TmuxPaneId> {
        None
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PaneItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    pane_id: TmuxPaneId,
    _pane_index: u64,
    cursor_x: u64,
    cursor_y: u64,
    pane_width: u64,
    pane_height: u64,
    pane_left: u64,
    pane_top: u64,
    pane_active: bool,
}

#[derive(Debug)]
struct WindowItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    window_width: u64,
    window_height: u64,
    window_active: bool,
    window_name: String,
    layout: Vec<WindowLayout>,
    layout_tree: LayoutNode,
    layout_csum: String,
    history_limit: isize,
}

struct PreparedTmuxPane {
    pane: Arc<dyn Pane>,
    remote: crate::tmux::RefTmuxRemotePane,
    committed: bool,
}

impl PreparedTmuxPane {
    fn commit(mut self) -> (Arc<dyn Pane>, crate::tmux::RefTmuxRemotePane) {
        self.committed = true;
        (Arc::clone(&self.pane), Arc::clone(&self.remote))
    }
}

impl Drop for PreparedTmuxPane {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let active_lock = Arc::clone(&self.remote.lock().active_lock);
        let (released, condvar) = &*active_lock;
        *released.lock() = true;
        condvar.notify_all();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PaneOwnership {
    pane_id: TmuxPaneId,
    window_id: TmuxWindowId,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TopologyReconciliationPlan {
    retained: HashSet<PaneOwnership>,
    created: HashSet<PaneOwnership>,
    removed: HashSet<PaneOwnership>,
    moved: HashSet<(PaneOwnership, PaneOwnership)>,
    unchanged_windows: HashSet<TmuxWindowId>,
    preserved_subtrees: HashSet<(TmuxWindowId, LayoutTopology)>,
}

fn pane_ownership(
    windows: &HashMap<TmuxWindowId, LayoutNode>,
) -> anyhow::Result<HashMap<TmuxPaneId, TmuxWindowId>> {
    let mut ownership = HashMap::new();
    for (window_id, layout) in windows {
        let mut pane_ids = vec![];
        layout.pane_ids(&mut pane_ids);
        for pane_id in pane_ids {
            if let Some(other_window) = ownership.insert(pane_id, *window_id) {
                anyhow::bail!(
                    "tmux pane %{pane_id} appears in both @{other_window} and @{window_id}"
                );
            }
        }
    }
    Ok(ownership)
}

fn reposition_is_reconciled(
    accepted: bool,
    source: TmuxPaneId,
    target: TmuxPaneId,
    target_window: TmuxWindowId,
    snapshot_ownership: &HashMap<TmuxPaneId, TmuxWindowId>,
) -> bool {
    accepted
        && snapshot_ownership.get(&source) == Some(&target_window)
        && snapshot_ownership.get(&target) == Some(&target_window)
}

fn plan_topology_reconciliation(
    current: &HashMap<TmuxWindowId, LayoutNode>,
    snapshot: &HashMap<TmuxWindowId, LayoutNode>,
) -> anyhow::Result<TopologyReconciliationPlan> {
    let current_ownership = pane_ownership(current)?;
    let snapshot_ownership = pane_ownership(snapshot)?;
    let mut plan = TopologyReconciliationPlan::default();

    for (pane_id, current_window) in &current_ownership {
        let current = PaneOwnership {
            pane_id: *pane_id,
            window_id: *current_window,
        };
        match snapshot_ownership.get(pane_id) {
            Some(snapshot_window) if snapshot_window == current_window => {
                plan.retained.insert(current);
            }
            Some(snapshot_window) => {
                plan.moved.insert((
                    current,
                    PaneOwnership {
                        pane_id: *pane_id,
                        window_id: *snapshot_window,
                    },
                ));
            }
            None => {
                plan.removed.insert(current);
            }
        }
    }

    for (pane_id, snapshot_window) in &snapshot_ownership {
        if !current_ownership.contains_key(pane_id) {
            plan.created.insert(PaneOwnership {
                pane_id: *pane_id,
                window_id: *snapshot_window,
            });
        }
    }

    for (window_id, snapshot_layout) in snapshot {
        if current
            .get(window_id)
            .is_some_and(|layout| layout.same_topology(snapshot_layout))
        {
            plan.unchanged_windows.insert(*window_id);
        }

        if let Some(current_layout) = current.get(window_id) {
            let mut current_subtrees = vec![];
            current_layout.split_topologies(&mut current_subtrees);
            let current_subtrees: HashSet<_> = current_subtrees.into_iter().collect();
            let mut snapshot_subtrees = vec![];
            snapshot_layout.split_topologies(&mut snapshot_subtrees);
            for subtree in snapshot_subtrees {
                if current_subtrees.contains(&subtree) {
                    plan.preserved_subtrees.insert((*window_id, subtree));
                }
            }
        }
    }

    Ok(plan)
}

fn clone_pane_tree(tree: &Tree) -> Tree {
    match tree {
        Tree::Empty => Tree::Empty,
        Tree::Leaf(pane) => Tree::Leaf(Arc::clone(pane)),
        Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(clone_pane_tree(left)),
            right: Box::new(clone_pane_tree(right)),
            data: *data,
        },
    }
}

fn local_tree_topology(
    tree: &Tree,
    remote_by_local: &HashMap<PaneId, TmuxPaneId>,
) -> anyhow::Result<LayoutTopology> {
    match tree {
        Tree::Empty => anyhow::bail!("local pane tree is empty"),
        Tree::Leaf(pane) => remote_by_local
            .get(&pane.pane_id())
            .copied()
            .map(LayoutTopology::Pane)
            .ok_or_else(|| anyhow!("local pane {} has no stable tmux id", pane.pane_id())),
        Tree::Node { left, right, data } => {
            let split = data
                .as_ref()
                .ok_or_else(|| anyhow!("local pane tree contains an unlabeled split"))?;
            let children = vec![
                local_tree_topology(left, remote_by_local)?,
                local_tree_topology(right, remote_by_local)?,
            ];
            Ok(match split.direction {
                SplitDirection::Horizontal => LayoutTopology::SplitHorizontal(children),
                SplitDirection::Vertical => LayoutTopology::SplitVertical(children),
            }
            .normalized())
        }
    }
}

fn collect_reusable_local_subtrees(
    tree: &Tree,
    remote_by_local: &HashMap<PaneId, TmuxPaneId>,
    subtrees: &mut HashMap<LayoutTopology, Tree>,
) -> anyhow::Result<LayoutTopology> {
    let topology = local_tree_topology(tree, remote_by_local)?;
    if let Tree::Node { left, right, .. } = tree {
        collect_reusable_local_subtrees(left, remote_by_local, subtrees)?;
        collect_reusable_local_subtrees(right, remote_by_local, subtrees)?;
        subtrees.insert(topology.clone(), clone_pane_tree(tree));
    }
    Ok(topology)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SplitPartition {
    horizontal: bool,
    left: Vec<TmuxPaneId>,
    right: Vec<TmuxPaneId>,
}

fn collect_local_split_partitions(
    tree: &Tree,
    remote_by_local: &HashMap<PaneId, TmuxPaneId>,
    partitions: &mut HashMap<SplitPartition, SplitDirectionAndSize>,
) -> anyhow::Result<Vec<TmuxPaneId>> {
    match tree {
        Tree::Empty => Ok(vec![]),
        Tree::Leaf(pane) => Ok(vec![*remote_by_local.get(&pane.pane_id()).ok_or_else(
            || anyhow!("local tmux pane {} has no remote id", pane.pane_id()),
        )?]),
        Tree::Node { left, right, data } => {
            let split = data.ok_or_else(|| anyhow!("local pane tree has unlabeled split"))?;
            let mut left_ids = collect_local_split_partitions(left, remote_by_local, partitions)?;
            let mut right_ids = collect_local_split_partitions(right, remote_by_local, partitions)?;
            left_ids.sort_unstable();
            right_ids.sort_unstable();
            partitions.insert(
                SplitPartition {
                    horizontal: split.direction == SplitDirection::Horizontal,
                    left: left_ids.clone(),
                    right: right_ids.clone(),
                },
                split,
            );
            left_ids.extend(right_ids);
            Ok(left_ids)
        }
    }
}

fn retained_layout_panes(layout: &LayoutNode, retained: &HashSet<TmuxPaneId>) -> Vec<TmuxPaneId> {
    let mut panes = vec![];
    layout.pane_ids(&mut panes);
    panes.retain(|pane| retained.contains(pane));
    panes.sort_unstable();
    panes
}

fn project_local_tree(
    tree: &Tree,
    remote_by_local: &HashMap<PaneId, TmuxPaneId>,
    retained: &HashSet<TmuxPaneId>,
) -> anyhow::Result<Option<Tree>> {
    match tree {
        Tree::Empty => Ok(None),
        Tree::Leaf(pane) => {
            let remote_id = remote_by_local
                .get(&pane.pane_id())
                .copied()
                .ok_or_else(|| anyhow!("local tmux pane {} has no remote id", pane.pane_id()))?;
            Ok(retained
                .contains(&remote_id)
                .then(|| Tree::Leaf(Arc::clone(pane))))
        }
        Tree::Node { left, right, data } => {
            let left = project_local_tree(left, remote_by_local, retained)?;
            let right = project_local_tree(right, remote_by_local, retained)?;
            Ok(match (left, right) {
                (Some(left), Some(right)) => Some(Tree::Node {
                    left: Box::new(left),
                    right: Box::new(right),
                    data: *data,
                }),
                (Some(tree), None) | (None, Some(tree)) => Some(tree),
                (None, None) => None,
            })
        }
    }
}

fn layout_geometry(layout: &LayoutNode) -> LayoutGeometry {
    match layout {
        LayoutNode::Pane(pane) => LayoutGeometry {
            width: pane.pane_width,
            height: pane.pane_height,
            left: pane.pane_left,
            top: pane.pane_top,
        },
        LayoutNode::SplitHorizontal { geometry, .. }
        | LayoutNode::SplitVertical { geometry, .. } => *geometry,
    }
}

fn collect_layout_panes(layout: &LayoutNode, panes: &mut Vec<PaneLayout>) {
    match layout {
        LayoutNode::Pane(pane) => panes.push(*pane),
        LayoutNode::SplitHorizontal { children, .. }
        | LayoutNode::SplitVertical { children, .. } => {
            for child in children {
                collect_layout_panes(child, panes);
            }
        }
    }
}

fn union_layout_geometry(children: &[LayoutNode]) -> anyhow::Result<LayoutGeometry> {
    let first = children
        .first()
        .map(layout_geometry)
        .ok_or_else(|| anyhow!("tmux split has no children"))?;
    let mut right = first.left.saturating_add(first.width);
    let mut bottom = first.top.saturating_add(first.height);
    let mut left = first.left;
    let mut top = first.top;
    for child in &children[1..] {
        let geometry = layout_geometry(child);
        left = left.min(geometry.left);
        top = top.min(geometry.top);
        right = right.max(geometry.left.saturating_add(geometry.width));
        bottom = bottom.max(geometry.top.saturating_add(geometry.height));
    }
    Ok(LayoutGeometry {
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
        left,
        top,
    })
}

fn terminal_size_for_geometry(
    geometry: LayoutGeometry,
    parent_geometry: LayoutGeometry,
    parent_size: TerminalSize,
) -> TerminalSize {
    TerminalSize {
        cols: geometry.width.max(1) as usize,
        rows: geometry.height.max(1) as usize,
        pixel_width: parent_size
            .pixel_width
            .saturating_mul(geometry.width as usize)
            / parent_geometry.width.max(1) as usize,
        pixel_height: parent_size
            .pixel_height
            .saturating_mul(geometry.height as usize)
            / parent_geometry.height.max(1) as usize,
        dpi: parent_size.dpi,
    }
}

fn build_snapshot_tree(
    layout: &LayoutNode,
    panes: &HashMap<TmuxPaneId, Arc<dyn Pane>>,
    reusable: &HashMap<LayoutTopology, Tree>,
    partitions: &HashMap<SplitPartition, SplitDirectionAndSize>,
    retained: &HashSet<TmuxPaneId>,
    size: TerminalSize,
) -> anyhow::Result<Tree> {
    let topology = layout.topology().normalized();
    if let Some(tree) = reusable.get(&topology) {
        return Ok(clone_pane_tree(tree));
    }

    match layout {
        LayoutNode::Pane(pane) => panes
            .get(&pane.pane_id)
            .map(|pane| Tree::Leaf(Arc::clone(pane)))
            .ok_or_else(|| anyhow!("tmux pane %{} has no local pane object", pane.pane_id)),
        LayoutNode::SplitHorizontal { geometry, children } => build_snapshot_split(
            children,
            SplitDirection::Horizontal,
            *geometry,
            panes,
            reusable,
            partitions,
            retained,
            size,
        ),
        LayoutNode::SplitVertical { geometry, children } => build_snapshot_split(
            children,
            SplitDirection::Vertical,
            *geometry,
            panes,
            reusable,
            partitions,
            retained,
            size,
        ),
    }
}

fn build_snapshot_split(
    children: &[LayoutNode],
    direction: SplitDirection,
    geometry: LayoutGeometry,
    panes: &HashMap<TmuxPaneId, Arc<dyn Pane>>,
    reusable: &HashMap<LayoutTopology, Tree>,
    partitions: &HashMap<SplitPartition, SplitDirectionAndSize>,
    retained: &HashSet<TmuxPaneId>,
    size: TerminalSize,
) -> anyhow::Result<Tree> {
    if children.len() < 2 {
        anyhow::bail!("tmux split must contain at least two children");
    }
    let first_geometry = layout_geometry(&children[0]);
    let remaining_geometry = union_layout_geometry(&children[1..])?;
    let first_size = terminal_size_for_geometry(first_geometry, geometry, size);
    let remaining_size = terminal_size_for_geometry(remaining_geometry, geometry, size);
    let left = build_snapshot_tree(
        &children[0],
        panes,
        reusable,
        partitions,
        retained,
        first_size,
    )?;
    let right = if children.len() == 2 {
        build_snapshot_tree(
            &children[1],
            panes,
            reusable,
            partitions,
            retained,
            remaining_size,
        )?
    } else {
        build_snapshot_split(
            &children[1..],
            direction,
            remaining_geometry,
            panes,
            reusable,
            partitions,
            retained,
            remaining_size,
        )?
    };
    let partition = SplitPartition {
        horizontal: direction == SplitDirection::Horizontal,
        left: retained_layout_panes(&children[0], retained),
        right: children[1..]
            .iter()
            .flat_map(|child| retained_layout_panes(child, retained))
            .collect(),
    };
    Ok(Tree::Node {
        left: Box::new(left),
        right: Box::new(right),
        data: Some(partitions.get(&partition).copied().unwrap_or_else(|| {
            SplitDirectionAndSize::from_snapshot(direction, first_size, remaining_size)
        })),
    })
}

impl TmuxDomainState {
    /// check if a PaneItem received from ListAllPanes has been attached
    pub fn check_pane_attached(&self, window_id: TmuxWindowId, pane_id: TmuxPaneId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        let Some(local_tab) = gui_tabs.get(&window_id) else {
            return false;
        };

        return local_tab.panes.get(&pane_id).is_some();
    }

    pub fn check_window_attached(&self, window_id: TmuxWindowId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        return gui_tabs.get(&window_id).is_some();
    }

    /// after we create a tab for a remote pane, save its ID into the
    /// TmuxPane-TmuxPane tree, so we can ref it later.
    fn add_attached_pane(
        &self,
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let panes = match gui_tabs.get_mut(&window_id) {
            Some(tab) => &mut tab.panes,
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        match panes.get(&pane_id) {
            Some(_) => {
                anyhow::bail!("Tmux pane already attached");
            }
            None => {
                panes.insert(pane_id);
                return Ok(());
            }
        }
    }

    fn pane_for_new_window(&self, pane: &PaneItem) -> anyhow::Result<(Arc<dyn Pane>, bool)> {
        if let Some(remote) = self.remote_panes.lock().get(&pane.pane_id).cloned() {
            let local_pane_id = remote.lock().local_pane_id;
            let local_pane = Mux::get()
                .get_pane(local_pane_id)
                .ok_or_else(|| anyhow!("local pane {local_pane_id} disappeared"))?;
            remote.lock().window_id = pane.window_id;
            for attached in self.gui_tabs.lock().values_mut() {
                attached.panes.remove(&pane.pane_id);
            }
            return Ok((local_pane, false));
        }
        Ok((
            self.create_pane(pane).context("failed to create pane")?,
            true,
        ))
    }

    fn add_attached_window(&self, target: &WindowItem, tab_id: &TabId) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();
        if !gui_tabs.contains_key(&target.window_id) {
            gui_tabs.insert(
                target.window_id,
                TmuxTab {
                    tab_id: *tab_id,
                    tmux_window_id: target.window_id,
                    layout_csum: target.layout_csum.clone(),
                    layout_tree: target.layout_tree.clone(),
                    panes: HashSet::new(),
                },
            );
        }

        Ok(())
    }

    fn remove_detached_pane(
        &self,
        window_id: TmuxWindowId,
        new_set: &HashSet<TmuxPaneId>,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let (tab_id, panes) = match gui_tabs.get_mut(&window_id) {
            Some(tab) => (tab.tab_id, &mut tab.panes),
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        let to_remove: Vec<_> = panes.difference(new_set).cloned().collect();

        let mux = Mux::get();
        for p in to_remove {
            let pane_map = self.remote_panes.lock();
            let Some(pane) = pane_map.get(&p) else {
                continue;
            };
            let pane = pane.lock();
            let local_pane_id = pane.local_pane_id;
            let (active, condvar) = &*pane.active_lock;
            *active.lock() = true;
            condvar.notify_all();
            drop(pane);
            drop(pane_map);
            if let Some(tab) = mux.get_tab(tab_id) {
                tab.remove_pane(local_pane_id);
            }
            mux.remove_pane_without_pruning(local_pane_id);
            self.remote_panes.lock().remove(&p);
            panes.remove(&p);
        }

        if panes.is_empty() {
            mux.remove_tab(tab_id);
            gui_tabs.remove(&window_id);
        }

        Ok(())
    }

    pub fn remove_detached_window(&self, window_id: TmuxWindowId) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();
        let tab = match gui_tabs.get(&window_id) {
            Some(x) => x,
            None => {
                anyhow::bail!("Cannot find the window {window_id}")
            }
        };

        // The snapshot diff below separately deregisters panes that are truly
        // absent.  Preserve all pane objects here so panes moved out of this
        // disappearing window retain their local identity and render state.
        Mux::get().remove_tab_preserving_panes(tab.tab_id);
        gui_tabs.remove(&window_id);

        Ok(())
    }

    fn set_pane_cursor_position(&self, pane: &Arc<dyn Pane>, x: usize, y: usize) {
        pane.perform_actions(vec![Action::CSI(CSI::Cursor(
            Cursor::CharacterAndLinePosition {
                col: OneBased::from_zero_based(x as u32),
                line: OneBased::from_zero_based(y as u32),
            },
        ))]);
    }

    fn create_pane(&self, pane: &PaneItem) -> anyhow::Result<Arc<dyn Pane>> {
        let (pane, ref_pane) = self.prepare_pane(pane)?.commit();
        let remote_id = ref_pane.lock().pane_id;
        self.remote_panes.lock().insert(remote_id, ref_pane);
        Ok(pane)
    }

    fn prepare_pane(&self, pane: &PaneItem) -> anyhow::Result<PreparedTmuxPane> {
        let local_pane_id = alloc_pane_id();
        log::debug!(
            "preparing tmux pane %{} as local pane {} in window @{}",
            pane.pane_id,
            local_pane_id,
            pane.window_id
        );
        let active_lock = Arc::new((Mutex::new(false), Condvar::new()));
        let (output_read, output_write) = filedescriptor::socketpair()?;
        let ref_pane = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id,
            output_write,
            active_lock: active_lock.clone(),
            session_id: 0,
            window_id: pane.window_id,
            pane_id: pane.pane_id,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            pane_width: pane.pane_width,
            pane_height: pane.pane_height,
            pane_left: pane.pane_left,
            pane_top: pane.pane_top,
        }));

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: self.cmd_queue.clone(),
            master_pane: Arc::clone(&ref_pane),
        };

        let writer = WriterWrapper::new(pane_pty.take_writer()?);

        let size = TerminalSize {
            rows: pane.pane_height as usize,
            cols: pane.pane_width as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };

        let child = TmuxChild {
            active_lock: active_lock.clone(),
        };

        let terminal = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );

        let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            local_pane_id,
            terminal,
            Box::new(child),
            Box::new(pane_pty),
            Box::new(writer),
            self.domain_id,
            "tmux pane".to_string(),
            None,
        ));
        Ok(PreparedTmuxPane {
            pane,
            remote: ref_pane,
            committed: false,
        })
    }

    fn sync_pane_state(&self, panes: &[PaneItem]) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = Mux::get();

        for pane in panes.iter() {
            if pane.session_id != current_session
                || !self.check_pane_attached(pane.window_id, pane.pane_id)
            {
                continue;
            }

            // We now have the cursor information, fix the cursor position
            let pane_map = self.remote_panes.lock();
            let local_pane = match pane_map.get(&pane.pane_id) {
                Some(p) => {
                    let local_pane_id = p.lock().local_pane_id;
                    mux.get_pane(local_pane_id)
                }
                None => None,
            };

            if let Some(local_pane) = local_pane {
                let c = local_pane.get_cursor_position();
                // no capture, output case
                if (c.x + c.y as usize) == 0 {
                    if let Some(text) = self.backlog.lock().remove(&pane.pane_id) {
                        if let Some(ref_pane) = pane_map.get(&pane.pane_id) {
                            let mut ref_pane = ref_pane.lock();
                            if let Err(err) = ref_pane.output_write.write_all(&text) {
                                log::error!("Failed to write tmux data to output: {:#}", err);
                            }
                        }
                    }
                } else {
                    // we have capture, so remove the backlog
                    let _ = self.backlog.lock().remove(&pane.pane_id);
                    if (pane.cursor_x + pane.cursor_y) != 0 {
                        self.set_pane_cursor_position(
                            &local_pane,
                            pane.cursor_x as usize,
                            pane.cursor_y as usize,
                        );
                    }
                }
                if pane.pane_active {
                    let gui_tabs = self.gui_tabs.lock();

                    let Some(local_tab) = gui_tabs.get(&pane.window_id) else {
                        anyhow::bail!("invalid tmux window id {}", pane.window_id);
                    };

                    match mux.get_tab(local_tab.tab_id) {
                        Some(tab) => {
                            tab.reconcile_active_pane(&local_pane);
                        }
                        None => {}
                    }
                }
            }

            log::info!("new pane synced, id: {}", pane.pane_id);
        }

        let completed_focus: Vec<_> = {
            let mut pending = self.pending_focus.lock();
            let mut completed = vec![];
            let mut index = 0;
            while index < pending.len() {
                let focus = &pending[index];
                let reconciled = focus.accepted
                    && panes
                        .iter()
                        .any(|pane| pane.pane_id == focus.pane_id && pane.pane_active);
                if reconciled {
                    if let Some(focus) = pending.remove(index) {
                        completed.push(focus.completion);
                    }
                } else {
                    index += 1;
                }
            }
            completed
        };
        for mut completion in completed_focus {
            completion.ok(());
        }

        Ok(())
    }

    fn sync_window_state(&self, windows: &[WindowItem], new_window: bool) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = Mux::get();

        let current_layouts: HashMap<TmuxWindowId, LayoutNode> = self
            .gui_tabs
            .lock()
            .iter()
            .map(|(window_id, tab)| (*window_id, tab.layout_tree.clone()))
            .collect();
        let snapshot_layouts: HashMap<TmuxWindowId, LayoutNode> = windows
            .iter()
            .filter(|window| window.session_id == current_session)
            .map(|window| (window.window_id, window.layout_tree.clone()))
            .collect();
        if should_ignore_empty_managed_snapshot(
            self.managed,
            current_layouts.len(),
            snapshot_layouts.len(),
        ) {
            log::warn!(
                "ignoring empty managed tmux snapshot for session ${current_session}; retaining {} attached windows and retrying",
                current_layouts.len()
            );
            let mut queue = self.cmd_queue.lock();
            if !queue
                .iter()
                .any(|command| command.is_full_window_snapshot())
            {
                queue.push_back(Box::new(ListAllWindows {
                    session_id: current_session,
                    window_id: None,
                }));
            }
            return Ok(());
        }
        let topology_plan = plan_topology_reconciliation(&current_layouts, &snapshot_layouts)?;
        log::debug!("tmux topology reconciliation plan: {topology_plan:#?}");

        let remote_by_local: HashMap<_, _> = self
            .remote_panes
            .lock()
            .iter()
            .map(|(remote_id, pane)| (pane.lock().local_pane_id, *remote_id))
            .collect();
        let mut reusable_subtrees = HashMap::new();
        let mut reusable_partitions = HashMap::new();
        for (window_id, attached) in self.gui_tabs.lock().iter() {
            let Some(tab) = mux.get_tab(attached.tab_id) else {
                continue;
            };
            let mut local_subtrees = HashMap::new();
            let mut local_partitions = HashMap::new();
            collect_local_split_partitions(
                &tab.snapshot_pane_tree(),
                &remote_by_local,
                &mut local_partitions,
            )?;
            reusable_partitions.insert(*window_id, local_partitions);
            collect_reusable_local_subtrees(
                &tab.snapshot_pane_tree(),
                &remote_by_local,
                &mut local_subtrees,
            )?;
            if let Some(snapshot) = snapshot_layouts.get(window_id) {
                let mut retained = vec![];
                snapshot.pane_ids(&mut retained);
                if let Some(projected) = project_local_tree(
                    &tab.snapshot_pane_tree(),
                    &remote_by_local,
                    &retained.into_iter().collect(),
                )? {
                    collect_reusable_local_subtrees(
                        &projected,
                        &remote_by_local,
                        &mut local_subtrees,
                    )?;
                }
            }
            if let Some(snapshot) = snapshot_layouts.get(window_id) {
                let mut snapshot_subtrees = vec![];
                snapshot.split_topologies(&mut snapshot_subtrees);
                for topology in snapshot_subtrees {
                    if let Some(tree) = local_subtrees.remove(&topology) {
                        reusable_subtrees.insert((*window_id, topology), tree);
                    }
                }
            }
        }
        log::debug!(
            "tmux reconciliation retained {} local split subtrees",
            reusable_subtrees.len()
        );

        let mut pane_objects: HashMap<_, _> = self
            .remote_panes
            .lock()
            .iter()
            .filter_map(|(remote_id, pane)| {
                mux.get_pane(pane.lock().local_pane_id)
                    .map(|pane| (*remote_id, pane))
            })
            .collect();
        if !new_window {
            let attached_windows: HashSet<_> = self.gui_tabs.lock().keys().copied().collect();
            let mut staged = vec![];
            for ownership in &topology_plan.created {
                // A pane can be temporarily absent from attached topology
                // after its source window is removed while its stable remote
                // transport and local pane object remain alive.  Reuse that
                // object when the next complete snapshot establishes its new
                // owner; only allocate for a genuinely unseen tmux pane id.
                if pane_objects.contains_key(&ownership.pane_id) {
                    continue;
                }
                if !attached_windows.contains(&ownership.window_id) {
                    continue;
                }
                let window = windows
                    .iter()
                    .find(|window| window.window_id == ownership.window_id)
                    .ok_or_else(|| anyhow!("missing snapshot window @{}", ownership.window_id))?;
                let mut layouts = vec![];
                collect_layout_panes(&window.layout_tree, &mut layouts);
                let layout = layouts
                    .iter()
                    .find(|pane| pane.pane_id == ownership.pane_id)
                    .ok_or_else(|| anyhow!("missing snapshot pane %{}", ownership.pane_id))?;
                let prepared = self.prepare_pane(&PaneItem {
                    session_id: window.session_id,
                    window_id: window.window_id,
                    pane_id: layout.pane_id,
                    _pane_index: 0,
                    cursor_x: 0,
                    cursor_y: 0,
                    pane_width: layout.pane_width,
                    pane_height: layout.pane_height,
                    pane_left: layout.pane_left,
                    pane_top: layout.pane_top,
                    pane_active: false,
                })?;
                pane_objects.insert(ownership.pane_id, Arc::clone(&prepared.pane));
                staged.push(prepared);
            }

            let mut candidates = vec![];
            for window in windows
                .iter()
                .filter(|window| window.session_id == current_session)
            {
                if topology_plan.unchanged_windows.contains(&window.window_id) {
                    continue;
                }
                let Some(tab_id) = self
                    .gui_tabs
                    .lock()
                    .get(&window.window_id)
                    .map(|attached| attached.tab_id)
                else {
                    continue;
                };
                let Some(tab) = mux.get_tab(tab_id) else {
                    continue;
                };
                let mut pane_ids = vec![];
                window.layout_tree.pane_ids(&mut pane_ids);
                let reusable = reusable_subtrees
                    .iter()
                    .filter_map(|((reusable_window, topology), tree)| {
                        (*reusable_window == window.window_id)
                            .then(|| (topology.clone(), clone_pane_tree(tree)))
                    })
                    .collect();
                let partitions = reusable_partitions
                    .get(&window.window_id)
                    .cloned()
                    .unwrap_or_default();
                let retained: HashSet<_> = remote_by_local.values().copied().collect();
                let tree = build_snapshot_tree(
                    &window.layout_tree,
                    &pane_objects,
                    &reusable,
                    &partitions,
                    &retained,
                    tab.get_size(),
                )?;
                let repositioned_active = self
                    .pending_repositions
                    .lock()
                    .iter()
                    .find(|operation| {
                        operation.accepted && operation.target_window == window.window_id
                    })
                    .map(|operation| operation.source)
                    .filter(|pane_id| pane_ids.contains(pane_id));
                let broken_active = self
                    .pending_breaks
                    .lock()
                    .iter()
                    .find(|operation| operation.accepted_window == Some(window.window_id))
                    .map(|operation| operation.source)
                    .filter(|pane_id| pane_ids.contains(pane_id));
                let current_active = repositioned_active
                    .or(broken_active)
                    .or_else(|| {
                        tab.get_active_pane()
                            .and_then(|pane| remote_by_local.get(&pane.pane_id()).copied())
                            .filter(|pane_id| pane_ids.contains(pane_id))
                    })
                    .or_else(|| pane_ids.first().copied())
                    .ok_or_else(|| anyhow!("snapshot window @{} has no panes", window.window_id))?;
                let active_local = pane_objects
                    .get(&current_active)
                    .map(|pane| pane.pane_id())
                    .ok_or_else(|| {
                        anyhow!("snapshot active pane %{current_active} is unavailable")
                    })?;
                Tab::validate_pane_tree(&tree, active_local)?;
                candidates.push((
                    window.window_id,
                    Arc::clone(&tab),
                    tree,
                    active_local,
                    window.layout_tree.clone(),
                    window.layout_csum.clone(),
                    pane_ids.into_iter().collect::<HashSet<_>>(),
                ));
            }
            log::debug!(
                "tmux reconciliation validated {} complete candidate trees and {} staged panes",
                candidates.len(),
                staged.len()
            );

            for prepared in staged {
                let (pane, remote) = prepared.commit();
                let remote_id = remote.lock().pane_id;
                self.remote_panes
                    .lock()
                    .insert(remote_id, Arc::clone(&remote));
                if let Err(err) = mux.add_pane(&pane) {
                    self.remote_panes.lock().remove(&remote_id);
                    let active_lock = Arc::clone(&remote.lock().active_lock);
                    let (released, condvar) = &*active_lock;
                    *released.lock() = true;
                    condvar.notify_all();
                    return Err(err).context("registering staged tmux pane");
                }
            }
            for (_, tab, tree, active_local, _, _, _) in &mut candidates {
                tab.replace_pane_tree_silently(
                    std::mem::replace(tree, Tree::Empty),
                    *active_local,
                )?;
            }

            {
                let mut tabs = self.gui_tabs.lock();
                for (window_id, _, _, _, layout, checksum, panes) in &candidates {
                    if let Some(attached) = tabs.get_mut(window_id) {
                        attached.layout_tree = layout.clone();
                        attached.layout_csum = checksum.clone();
                        attached.panes = panes.clone();
                    }
                }
            }
            {
                let pane_map = self.remote_panes.lock();
                for window in windows
                    .iter()
                    .filter(|window| window.session_id == current_session)
                {
                    let mut layouts = vec![];
                    collect_layout_panes(&window.layout_tree, &mut layouts);
                    for layout in layouts {
                        if let Some(pane) = pane_map.get(&layout.pane_id) {
                            pane.lock().window_id = window.window_id;
                        }
                    }
                }
            }
            let snapshot_window_ids: HashSet<_> = windows
                .iter()
                .filter(|window| window.session_id == current_session)
                .map(|window| window.window_id)
                .collect();
            let detached_windows: Vec<_> = self
                .gui_tabs
                .lock()
                .keys()
                .filter(|window_id| !snapshot_window_ids.contains(window_id))
                .copied()
                .collect();
            for window_id in detached_windows {
                self.remove_detached_window(window_id)?;
            }
            let completed_window_kills: Vec<_> = {
                let mut pending = self.pending_window_kills.lock();
                let completed: Vec<_> = pending
                    .keys()
                    .filter(|window_id| !snapshot_window_ids.contains(window_id))
                    .copied()
                    .collect();
                completed
                    .into_iter()
                    .filter_map(|window_id| pending.remove(&window_id))
                    .collect()
            };
            for removed in &topology_plan.removed {
                log::debug!(
                    "authoritative snapshot removing tmux pane %{} from window @{}",
                    removed.pane_id,
                    removed.window_id
                );
                if let Some(remote) = self.remote_panes.lock().remove(&removed.pane_id) {
                    let remote = remote.lock();
                    let local_pane_id = remote.local_pane_id;
                    let (released, condvar) = &*remote.active_lock;
                    *released.lock() = true;
                    condvar.notify_all();
                    drop(remote);
                    mux.remove_pane_without_pruning(local_pane_id);
                }
                let completion = self.pending_kills.lock().remove(&removed.pane_id);
                if let Some(mut completion) = completion {
                    completion.ok(());
                }
            }
            let snapshot_ownership = windows
                .iter()
                .filter(|window| window.session_id == current_session)
                .flat_map(|window| {
                    let mut pane_ids = vec![];
                    window.layout_tree.pane_ids(&mut pane_ids);
                    pane_ids
                        .into_iter()
                        .map(move |pane_id| (pane_id, window.window_id))
                })
                .collect::<HashMap<_, _>>();
            let mut completed_repositions = vec![];
            let mut completed_breaks = vec![];
            {
                let mut pending = self.pending_repositions.lock();
                let mut index = 0;
                while index < pending.len() {
                    let operation = &pending[index];
                    let reconciled = reposition_is_reconciled(
                        operation.accepted,
                        operation.source,
                        operation.target,
                        operation.target_window,
                        &snapshot_ownership,
                    );
                    if reconciled {
                        if let Some(operation) = pending.remove(index) {
                            self.pending_reposition_windows
                                .lock()
                                .remove(&operation.source_window);
                            completed_repositions.push(operation.completion);
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            {
                let mut pending = self.pending_breaks.lock();
                let mut index = 0;
                while index < pending.len() {
                    let operation = &pending[index];
                    let reconciled = operation.accepted_window.is_some_and(|window_id| {
                        snapshot_ownership.get(&operation.source) == Some(&window_id)
                            && self.gui_tabs.lock().contains_key(&window_id)
                    });
                    if reconciled {
                        if let Some(operation) = pending.remove(index) {
                            completed_breaks
                                .push((operation.completion, operation.accepted_window.unwrap()));
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            for (_, tab, _, active_local, _, _, _) in candidates {
                mux.notify(MuxNotification::TabResized(tab.tab_id()));
                mux.notify(MuxNotification::PaneFocused(active_local));
            }
            for mut completion in completed_repositions {
                completion.ok(());
            }
            for mut completion in completed_window_kills {
                completion.ok(());
            }
            for (mut completion, window_id) in completed_breaks {
                completion.ok(window_id);
            }
        }

        self.create_gui_window();
        let mut gui_window = self.gui_window.lock();
        let gui_window_id = match gui_window.as_mut() {
            Some(x) => x,
            None => {
                anyhow::bail!("No tmux gui created");
            }
        };

        for window in windows.iter() {
            if window.session_id != current_session {
                continue;
            }

            if let Some(existing) = self.gui_tabs.lock().get(&window.window_id) {
                if !existing.layout_tree.same_topology(&window.layout_tree) {
                    log::debug!(
                        "tmux window {} snapshot topology changed; scheduling pane synchronization",
                        window.window_id
                    );
                }
                if let Some(tab) = mux.get_tab(existing.tab_id) {
                    tab.set_title(&window.window_name);
                }
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
                continue;
            }

            let size = TerminalSize {
                rows: window.window_height as usize,
                cols: window.window_width as usize,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            };

            let tab = Arc::new(Tab::new(&size));
            tab.set_title(&format!("{}", &window.window_name));
            mux.add_tab_no_panes(&tab);

            let _ = self.add_attached_window(window, &tab.tab_id())?;

            let mut split_stack;
            let mut split_direction;

            let mut split_pane_index = 1;
            for l in &window.layout {
                match l {
                    WindowLayout::SinglePane(x) => {
                        let p = PaneItem {
                            session_id: window.session_id,
                            window_id: window.window_id,
                            _pane_index: 0,
                            cursor_x: 0,
                            cursor_y: 0,
                            pane_active: false,
                            pane_id: x.pane_id,
                            pane_width: x.pane_width,
                            pane_height: x.pane_height,
                            pane_left: x.pane_left,
                            pane_top: x.pane_top,
                        };
                        let (local_pane, created) = self.pane_for_new_window(&p)?;
                        tab.assign_pane(&local_pane);
                        local_pane.resize(size)?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        if created {
                            let _ = mux.add_pane(&local_pane);
                        }
                        break;
                    }

                    WindowLayout::SplitHorizontal(x) => {
                        split_direction = SplitDirection::Horizontal;
                        split_stack = x;
                    }

                    WindowLayout::SplitVertical(x) => {
                        split_direction = SplitDirection::Vertical;
                        split_stack = x;
                    }
                }

                for x in split_stack {
                    let p = PaneItem {
                        session_id: window.session_id,
                        window_id: window.window_id,
                        _pane_index: 0,
                        cursor_x: 0,
                        cursor_y: 0,
                        pane_active: false,
                        pane_id: x.pane_id,
                        pane_width: x.pane_width,
                        pane_height: x.pane_height,
                        pane_left: x.pane_left,
                        pane_top: x.pane_top,
                    };
                    let local_pane;
                    if !self.check_pane_attached(p.window_id, p.pane_id) {
                        let created;
                        (local_pane, created) = self.pane_for_new_window(&p)?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        if created {
                            let _ = mux.add_pane(&local_pane);
                        }
                        if let None = tab.get_active_pane() {
                            tab.assign_pane(&local_pane);
                            local_pane.resize(size)?;
                            split_pane_index = tab.get_active_idx();
                            continue;
                        }

                        let pane_count = tab.iter_panes_ignoring_zoom().len();
                        split_pane_index = tab.split_and_insert(
                            split_pane_index,
                            SplitRequest {
                                direction: split_direction,
                                target_is_second: false,
                                top_level: pane_count == 1,
                                size: SplitSize::Cells(
                                    if split_direction == SplitDirection::Horizontal {
                                        p.pane_width as usize
                                    } else {
                                        p.pane_height as usize
                                    },
                                ),
                            },
                            local_pane.clone(),
                        )
                        .with_context(|| {
                            format!(
                                "reconstructing tmux window {} pane {} at local index {} with {} existing panes",
                                p.window_id, p.pane_id, split_pane_index, pane_count
                            )
                        })? + 1;
                    } else {
                        let pane_map = self.remote_panes.lock();
                        let local_pane_id = match pane_map.get(&p.pane_id) {
                            Some(x) => x.lock().local_pane_id,
                            None => anyhow::bail!("cannot find the local pane for {}", p.pane_id),
                        };

                        split_pane_index = match tab
                            .iter_panes_ignoring_zoom()
                            .iter()
                            .find(|x| x.pane.pane_id() == local_pane_id)
                        {
                            Some(x) => x.index,
                            None => {
                                log::info!("invalid pane id {local_pane_id}");
                                continue;
                            }
                        };
                        continue;
                    }
                }
            }

            mux.add_tab_to_window(&tab, **gui_window_id)?;
            gui_window_id.notify();

            if new_window {
                let pending = self.pending_new_tabs.lock().pop_front();
                if let Some(mut pending) = pending {
                    pending.ok(Arc::clone(&tab));
                }
            }

            let gui_tabs = self.gui_tabs.lock();
            let local_tab = match gui_tabs.get(&window.window_id) {
                Some(x) => x,
                None => {
                    log::info!(
                        "cannot find the local tab for tmux window {}",
                        window.window_id
                    );
                    continue;
                }
            };

            // For new window, we wait for nature ouput instead of capturing
            if !new_window {
                for p in local_tab.panes.iter() {
                    self.cmd_queue.lock().push_back(Box::new(CapturePane {
                        pane_id: *p,
                        history_limit: window.history_limit,
                    }));
                }
            }

            // To keep the active window last one to make it active after set the focus pane
            if !window.window_active {
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
        }

        // To keep the active window last one to make it active after set the focus pane
        match windows.iter().find(|w| w.window_active) {
            Some(window) => {
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
            None => {}
        }

        if *self.attach_state.lock() == AttachState::Init {
            self.cmd_queue.lock().push_back(Box::new(AttachDone));
        }

        let completed_splits: Vec<_> = {
            let remote_panes = self.remote_panes.lock();
            let mut pending = self.pending_splits.lock();
            let mut completed = vec![];
            let mut index = 0;
            while index < pending.len() {
                let remote_id = pending[index].remote_id;
                let reconciled = remote_id
                    .and_then(|remote_id| remote_panes.get(&remote_id))
                    .map(|pane| pane.lock().local_pane_id)
                    .and_then(|local_id| mux.get_pane(local_id))
                    .is_some();
                if reconciled {
                    completed.push(pending.remove(index).expect("pending split index"));
                } else {
                    index += 1;
                }
            }
            completed
        };
        for mut split in completed_splits {
            split
                .completion
                .ok(split.remote_id.expect("reconciled split remote id"));
        }

        let completed_renames: Vec<_> = {
            let mut pending = self.pending_renames.lock();
            let mut completed = vec![];
            let mut index = 0;
            while index < pending.len() {
                let rename = &pending[index];
                let reconciled = rename.accepted
                    && windows.iter().any(|window| {
                        window.window_id == rename.window_id && window.window_name == rename.title
                    });
                if reconciled {
                    if let Some(rename) = pending.remove(index) {
                        completed.push(rename.completion);
                    }
                } else {
                    index += 1;
                }
            }
            completed
        };
        for mut completion in completed_renames {
            completion.ok(());
        }

        TmuxDomainState::schedule_send_next_command(self.domain_id);

        Ok(())
    }

    pub fn subscribe_notification(&self) {
        let mux = Mux::get();
        let domain_id = self.domain_id;
        mux.subscribe(move |n| {
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                let domain = match mux.get_domain(domain_id) {
                    Some(d) => d,
                    None => return,
                };
                let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
                    Some(t) => t,
                    None => return,
                };

                if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
                    return;
                }

                match n {
                    MuxNotification::PaneFocused(pane_id) => {
                        let tmux_pane_id = match tmux_domain
                            .inner
                            .remote_panes
                            .lock()
                            .iter()
                            .find(|(_, p)| p.lock().local_pane_id == pane_id)
                        {
                            Some((_, p)) => Some(p.lock().pane_id),
                            None => None,
                        };

                        if let Some(pane_id) = tmux_pane_id {
                            if tmux_domain
                                .inner
                                .pending_kills
                                .lock()
                                .contains_key(&pane_id)
                            {
                                return;
                            }
                            tmux_domain.inner.retain_pending_commands(|command| {
                                command.select_pane_id() != Some(pane_id)
                            });
                            let mut queue = tmux_domain.inner.cmd_queue.lock();
                            queue.push_back(Box::new(SelectPane { pane_id }));
                            drop(queue);
                            TmuxDomainState::schedule_send_next_command(domain_id);
                        }
                    }
                    MuxNotification::WindowInvalidated(window_id) => {
                        if let Some(window) = mux.get_window(window_id) {
                            let Some(tab) = window.get_active() else {
                                return;
                            };
                            let tmux_window_id = match tmux_domain
                                .inner
                                .gui_tabs
                                .lock()
                                .iter()
                                .find(|(_, t)| t.tab_id == tab.tab_id())
                            {
                                Some((_, t)) => Some(t.tmux_window_id),
                                None => None,
                            };
                            if let Some(window_id) = tmux_window_id {
                                tmux_domain.inner.cmd_queue.lock().push_back(Box::new(
                                    SelectWindow {
                                        window_id: window_id,
                                    },
                                ));
                                TmuxDomainState::schedule_send_next_command(domain_id);
                            }
                        }
                    }
                    _ => {}
                }
            })
            .detach();
            true
        });
    }
}

fn should_ignore_empty_managed_snapshot(
    managed: bool,
    attached_windows: usize,
    snapshot_windows: usize,
) -> bool {
    managed && attached_windows > 0 && snapshot_windows == 0
}

fn parse_sigil_number(text: &str) -> anyhow::Result<u64> {
    let num = text
        .get(1..)
        .ok_or_else(|| anyhow!("wrong prefixed id"))?
        .parse()?;

    Ok(num)
}

#[derive(Debug)]
pub(crate) struct ListAllPanes {
    pub window_id: TmuxWindowId,
    pub prune: bool,
    pub layout_csum: String,
}

impl TmuxCommand for ListAllPanes {
    fn get_command(&self, domain_id: DomainId) -> String {
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        let mut gui_tabs = tmux_domain.inner.gui_tabs.lock();

        let Some(local_tab) = gui_tabs.get_mut(&self.window_id) else {
            return "".to_string();
        };

        if local_tab.layout_csum.eq(&self.layout_csum) {
            if self.prune {
                return "".to_string();
            }
        } else {
            local_tab.layout_csum = self.layout_csum.clone();
        }

        format!(
            "list-panes -F '#{{session_id}}\x1f#{{window_id}}\x1f#{{pane_id}}\x1f\
            #{{pane_index}}\x1f#{{cursor_x}}\x1f#{{cursor_y}}\x1f#{{pane_width}}\x1f#{{pane_height}}\x1f\
            #{{pane_left}}\x1f#{{pane_top}}\x1f#{{pane_active}}' -t @{}\n",
            self.window_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mut items = vec![];
        let mut pane_set = HashSet::new();
        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split('\x1f');
            // These ids all have various sigils such as `$`, `%`, `@`,
            // so skip those prior to parsing them
            let session_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
            let window_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
            let pane_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing pane_id"))?)?;
            let _pane_index = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_index"))?
                .parse()?;
            let cursor_x = fields
                .next()
                .ok_or_else(|| anyhow!("missing cursor_x"))?
                .parse()?;
            let cursor_y = fields
                .next()
                .ok_or_else(|| anyhow!("missing cursor_y"))?
                .parse()?;
            let pane_width = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_width"))?
                .parse()?;
            let pane_height = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_height"))?
                .parse()?;
            let pane_left = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_left"))?
                .parse()?;
            let pane_top = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_top"))?
                .parse()?;
            let pane_active = fields
                .next()
                .ok_or_else(|| anyhow!("missing pane_active"))?
                .parse::<usize>()?;

            let pane_active = pane_active == 1;

            pane_set.insert(pane_id);

            items.push(PaneItem {
                session_id,
                window_id,
                pane_id,
                _pane_index,
                cursor_x,
                cursor_y,
                pane_width,
                pane_height,
                pane_left,
                pane_top,
                pane_active,
            });
        }

        log::debug!("panes in domain_id {}: {:?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                if !self.prune {
                    return tmux_domain.inner.sync_pane_state(&items);
                } else {
                    return tmux_domain
                        .inner
                        .remove_detached_pane(self.window_id, &pane_set);
                }
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct ListAllWindows {
    pub session_id: TmuxSessionId,
    pub window_id: Option<TmuxWindowId>,
}

fn parse_window_snapshot(output: &str) -> anyhow::Result<Vec<WindowItem>> {
    let mut items = vec![];
    for line in output.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\x1f');
        let session_id =
            parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
        let window_id =
            parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
        let window_width = fields
            .next()
            .ok_or_else(|| anyhow!("missing window_width"))?
            .parse()?;
        let window_height = fields
            .next()
            .ok_or_else(|| anyhow!("missing window_height"))?
            .parse()?;
        let window_active = fields
            .next()
            .ok_or_else(|| anyhow!("missing window_active"))?
            .parse::<usize>()?
            == 1;
        let window_name = fields
            .next()
            .ok_or_else(|| anyhow!("missing window_name"))?;
        let window_layout = fields
            .next()
            .ok_or_else(|| anyhow!("missing window_layout"))?;
        let history_limit = fields
            .next()
            .ok_or_else(|| anyhow!("missing history_limit"))?
            .parse::<isize>()?;
        let layout_csum = window_layout
            .get(0..4)
            .ok_or_else(|| anyhow!("missing window_layout"))?;
        let window_layout = window_layout
            .get(5..)
            .ok_or_else(|| anyhow!("missing window_layout"))?;

        items.push(WindowItem {
            session_id,
            window_id,
            window_width,
            window_height,
            window_active,
            window_name: window_name.to_string(),
            layout: parse_layout(window_layout)?,
            layout_tree: parse_layout_tree(window_layout)?,
            layout_csum: layout_csum.to_string(),
            history_limit,
        });
    }
    Ok(items)
}

impl TmuxCommand for ListAllWindows {
    fn is_full_window_snapshot(&self) -> bool {
        true
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "list-windows -F \
                '#{{session_id}}\x1f#{{window_id}}\x1f\
                #{{window_width}}\x1f#{{window_height}}\x1f\
                #{{window_active}}\x1f\
                #{{window_name}}\x1f\
                #{{window_layout}}\x1f\
                #{{history_limit}}' -t ${}\n",
            self.session_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let items = parse_window_snapshot(&result.output)?;

        log::debug!("layout in domain_id {}: {:#?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                let new_window = if let Some(_x) = self.window_id {
                    true
                } else {
                    false
                };
                return tmux_domain.inner.sync_window_state(&items, new_window);
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct Resize {
    pub pane_id: TmuxPaneId,
    pub size: PtySize,
}

fn resize_command(window_command: String, pane_id: TmuxPaneId, size: PtySize) -> String {
    format!(
        "{window_command} ; resize-pane -x {} -y {} -t %{pane_id}\n",
        size.cols, size.rows
    )
}

impl TmuxCommand for Resize {
    fn guarded_response_count(&self) -> usize {
        2
    }

    fn resize_pane_id(&self) -> Option<TmuxPaneId> {
        Some(self.pane_id)
    }

    fn is_stale_for_killed_pane(&self, pane_id: TmuxPaneId) -> bool {
        self.pane_id == pane_id
    }

    fn get_command(&self, domain_id: DomainId) -> String {
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        // Not in stable state for now, don't do resizing, otherwise it will cause tmux output
        // unexpected content.
        if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
            return "".to_string();
        }

        let pane_map = tmux_domain.inner.remote_panes.lock();
        {
            let mut pane = match pane_map.get(&self.pane_id) {
                Some(x) => x.lock(),
                None => return "".to_string(),
            };

            if pane.pane_width == self.size.cols as u64 && pane.pane_height == self.size.rows as u64
            {
                return "".to_string();
            } else {
                pane.pane_width = self.size.cols as u64;
                pane.pane_height = self.size.rows as u64;
            }
        }

        let tmux_window_id = match pane_map.get(&self.pane_id) {
            Some(x) => x.lock().window_id,
            None => return "".to_string(),
        };

        let gui_tabs = tmux_domain.inner.gui_tabs.lock();
        let local_tab = match gui_tabs.get(&tmux_window_id) {
            Some(t) => t,
            None => return "".to_string(),
        };

        let size = match mux.get_tab(local_tab.tab_id) {
            Some(x) => x.get_size(),
            None => return "".to_string(),
        };

        let support_commands = tmux_domain.inner.support_commands.lock();

        let command = if let Some(_x) = support_commands.get("resize-window") {
            resize_command(
                format!(
                    "resize-window -x {} -y {} -t @{}",
                    size.cols, size.rows, tmux_window_id
                ),
                self.pane_id,
                self.size,
            )
        } else if let Some(x) = support_commands.get("refresh-client") {
            if x.contains("-C XxY") {
                resize_command(
                    format!("refresh-client -C {}x{}", size.cols, size.rows),
                    self.pane_id,
                    self.size,
                )
            } else {
                resize_command(
                    format!("refresh-client -C {},{}", size.cols, size.rows),
                    self.pane_id,
                    self.size,
                )
            }
        } else {
            log::info!("The tmux version is not supported");
            return "".to_string();
        };
        tmux_domain
            .inner
            .pending_capture_refresh
            .lock()
            .insert(self.pane_id);
        command
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                    tmux_domain
                        .inner
                        .pending_capture_refresh
                        .lock()
                        .remove(&self.pane_id);
                }
            }
            let error = format!("resize-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }

        // A resize can make tmux emit readline/application redraw output while
        // the local mirror is changing dimensions.  Finish with a fresh
        // authoritative capture so those relative redraws cannot leave the
        // mirror different from tmux's screen.
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                tmux_domain
                    .inner
                    .cmd_queue
                    .lock()
                    .push_back(Box::new(CapturePane {
                        pane_id: self.pane_id,
                        history_limit: config::configuration().scrollback_lines as isize,
                    }));
            }
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                tmux_domain
                    .inner
                    .pending_capture_refresh
                    .lock()
                    .remove(&self.pane_id);
            }
        }
        anyhow::bail!("tmux resize command timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct CapturePane {
    pane_id: TmuxPaneId,
    history_limit: isize,
}

impl TmuxCommand for CapturePane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "capture-pane -p -t %{} -e -C -S {}\n",
            self.pane_id,
            self.history_limit * -1
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                    tmux_domain
                        .inner
                        .pending_capture_refresh
                        .lock()
                        .remove(&self.pane_id);
                }
            }
            let error = format!("capture-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let unescaped = termwiz::tmux_cc::unvis(&result.output).context("unescape pane content")?;
        // capturep contents returned from guarded lines which always contain a tailing '\n'
        let unescaped = &unescaped[0..unescaped.len().saturating_sub(1)].replace("\n", "\r\n");

        // Control events are processed serially, so removing the barrier here
        // still keeps incremental output out until the replacement below has
        // completed, while ensuring write failures cannot strand the pane.
        tmux_domain
            .inner
            .pending_capture_refresh
            .lock()
            .remove(&self.pane_id);

        let pane_map = tmux_domain.inner.remote_panes.lock();
        if let Some(pane) = pane_map.get(&self.pane_id) {
            let mut pane = pane.lock();
            if let Some(p) = mux.get_pane(pane.local_pane_id) {
                tmux_domain.inner.set_pane_cursor_position(&p, 0, 0);
            }

            // This capture is an authoritative replacement for the terminal
            // contents.  Merely homing the cursor and painting over the old
            // screen leaves stale rows behind whenever reflow makes the new
            // capture shorter (most visibly as duplicated shell prompts after
            // changing an individual pane's font size).
            pane.output_write
                .write_all(b"\x1b[3J\x1b[2J\x1b[H")
                .context("clearing pane before authoritative capture")?;
            pane.output_write
                .write_all(unescaped.as_bytes())
                .context("writing capture pane result to output")?;
        }

        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                tmux_domain
                    .inner
                    .pending_capture_refresh
                    .lock()
                    .remove(&self.pane_id);
            }
        }
        anyhow::bail!("tmux capture-pane command timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct SendKeys {
    pub keys: Vec<u8>,
    pub pane: TmuxPaneId,
}
impl TmuxCommand for SendKeys {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let mut s = String::new();
        for &byte in self.keys.iter() {
            write!(&mut s, "0x{:X} ", byte).expect("unable to write key");
        }
        format!("send-keys -H -t %{} {}\n", self.pane, s)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("send-key in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NewWindow {
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
}
impl TmuxCommand for NewWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let mut command = "new-window -P -F '#{window_id}'".to_owned();
        if let Some(cwd) = &self.cwd {
            write!(&mut command, " -c {}", shell_words::quote(cwd)).unwrap();
        }
        if let Some(argv) = &self.command {
            if !argv.is_empty() {
                let shell_command = argv
                    .iter()
                    .map(|arg| shell_words::quote(arg).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ");
                write!(&mut command, " {}", shell_words::quote(&shell_command)).unwrap();
            }
        }
        command.push('\n');
        command
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    let pending = tmux.inner.pending_new_tabs.lock().pop_front();
                    if let Some(mut pending) = pending {
                        pending.err(anyhow!("tmux rejected new-window"));
                    }
                }
            }
            let error = format!("new-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let window_id = parse_sigil_number(result.output.trim())?;
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(session_id) = *tmux.inner.tmux_session.lock() {
                    let mut queue = tmux.inner.cmd_queue.lock();
                    if !queue
                        .iter()
                        .any(|command| command.is_full_window_snapshot())
                    {
                        queue.push_back(Box::new(ListAllWindows {
                            session_id,
                            window_id: Some(window_id),
                        }));
                    }
                }
            }
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let pending = tmux.inner.pending_new_tabs.lock().pop_front();
                if let Some(mut pending) = pending {
                    pending.err(anyhow!("tmux new-window timed out"));
                }
            }
        }
        anyhow::bail!("new-window timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct ListCommands;
impl TmuxCommand for ListCommands {
    fn get_command(&self, _domain_id: DomainId) -> String {
        "list-commands\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-command in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let mut support_commands = tmux_domain.inner.support_commands.lock();

        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let v: Vec<&str> = line.split(' ').collect();
            support_commands.insert(v[0].to_string(), line.to_string());
        }

        let mut cmd_queue = tmux_domain.inner.cmd_queue.as_ref().lock();
        if let Some(session) = *tmux_domain.inner.tmux_session.lock() {
            cmd_queue.push_back(Box::new(ListAllWindows {
                session_id: session,
                window_id: None,
            }));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SplitPane {
    pub operation_id: u64,
    pub pane_id: TmuxPaneId,
    pub direction: SplitDirection,
}

#[derive(Debug)]
pub(crate) struct KillPane {
    pub pane_id: TmuxPaneId,
}

#[derive(Debug)]
pub(crate) struct KillWindow {
    pub window_id: TmuxWindowId,
}

impl TmuxCommand for KillWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("kill-window -t @{}\n", self.window_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    let completion = tmux
                        .inner
                        .pending_window_kills
                        .lock()
                        .remove(&self.window_id);
                    if let Some(mut completion) = completion {
                        completion.err(anyhow!("tmux rejected kill-window"));
                    }
                }
            }
            anyhow::bail!("kill-window in domain={domain_id} failed: {result:#?}");
        }
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(session_id) = *tmux.inner.tmux_session.lock() {
                    tmux.inner
                        .cmd_queue
                        .lock()
                        .push_front(Box::new(ListAllWindows {
                            session_id,
                            window_id: None,
                        }));
                    TmuxDomainState::schedule_send_next_command(domain_id);
                }
            }
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let completion = tmux
                    .inner
                    .pending_window_kills
                    .lock()
                    .remove(&self.window_id);
                if let Some(mut completion) = completion {
                    completion.err(anyhow!("tmux kill-window timed out"));
                }
            }
        }
        anyhow::bail!("kill-window timed out in domain {domain_id}")
    }
}

impl TmuxCommand for KillPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("kill-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let already_absent = result.error
            && (result.output.contains("can't find pane")
                || result.output.contains("no such pane"));
        if result.error && !already_absent {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    let completion = tmux.inner.pending_kills.lock().remove(&self.pane_id);
                    if let Some(mut completion) = completion {
                        completion.err(anyhow!("tmux rejected kill-pane"));
                    }
                }
            }
            anyhow::bail!("kill-pane in domain={domain_id} failed: {result:#?}");
        }
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(session_id) = *tmux.inner.tmux_session.lock() {
                    tmux.inner
                        .cmd_queue
                        .lock()
                        .push_front(Box::new(ListAllWindows {
                            session_id,
                            window_id: None,
                        }));
                    TmuxDomainState::schedule_send_next_command(domain_id);
                }
            }
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let completion = tmux.inner.pending_kills.lock().remove(&self.pane_id);
                if let Some(mut completion) = completion {
                    completion.err(anyhow!("tmux kill-pane timed out"));
                }
            }
        }
        anyhow::bail!("kill-pane timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct BreakPane {
    pub operation_id: u64,
    pub source: TmuxPaneId,
}

impl TmuxCommand for BreakPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("break-pane -d -P -F '#{{window_id}}' -s %{}\n", self.source)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let domain = Mux::get()
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("tmux domain lost"))?;
        let tmux = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("tmux domain lost"))?;
        if result.error {
            tmux.inner.break_pane_failed(self.operation_id);
            anyhow::bail!("break-pane in domain={domain_id} failed: {result:#?}");
        }
        let window_id = parse_sigil_number(result.output.trim())
            .context("break-pane did not return the new window id")?;
        if let Some(pending) = tmux
            .inner
            .pending_breaks
            .lock()
            .iter_mut()
            .find(|pending| pending.operation_id == self.operation_id)
        {
            pending.accepted_window = Some(window_id);
        }
        if let Some(session_id) = *tmux.inner.tmux_session.lock() {
            tmux.inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListAllWindows {
                    session_id,
                    window_id: None,
                }));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                tmux.inner.break_pane_failed(self.operation_id);
            }
        }
        anyhow::bail!("break-pane timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct JoinPane {
    pub operation_id: u64,
    pub source: TmuxPaneId,
    pub source_window: TmuxWindowId,
    pub target: TmuxPaneId,
    pub request: SplitRequest,
}

impl TmuxCommand for JoinPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let orientation = match self.request.direction {
            SplitDirection::Horizontal => "-h",
            SplitDirection::Vertical => "-v",
        };
        let before = if self.request.target_is_second {
            ""
        } else {
            "-b "
        };
        format!(
            "join-pane {orientation} {before}-p 50 -s %{} -t %{}\n",
            self.source, self.target
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let mux = Mux::get();
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("tmux domain lost"))?;
        let tmux = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("tmux domain lost"))?;
        if result.error {
            tmux.inner
                .pane_reposition_failed(self.operation_id, self.source_window);
            anyhow::bail!("join-pane in domain={domain_id} failed: {result:#?}");
        }
        if let Some(pending) = tmux
            .inner
            .pending_repositions
            .lock()
            .iter_mut()
            .find(|pending| pending.operation_id == self.operation_id)
        {
            pending.accepted = true;
        }
        if let Some(session_id) = *tmux.inner.tmux_session.lock() {
            tmux.inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListAllWindows {
                    session_id,
                    window_id: None,
                }));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                tmux.inner
                    .pane_reposition_failed(self.operation_id, self.source_window);
            }
        }
        anyhow::bail!("join-pane timed out in domain {domain_id}")
    }
}

impl TmuxCommand for SplitPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        if self.direction == SplitDirection::Horizontal {
            format!(
                "split-window -h -P -F '#{{pane_id}}' -t %{}\n",
                self.pane_id
            )
        } else {
            format!(
                "split-window -v -P -F '#{{pane_id}}' -t %{}\n",
                self.pane_id
            )
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    let completion = {
                        let mut pending = tmux.inner.pending_splits.lock();
                        pending
                            .iter()
                            .position(|operation| operation.operation_id == self.operation_id)
                            .and_then(|index| pending.remove(index))
                            .map(|operation| operation.completion)
                    };
                    if let Some(mut completion) = completion {
                        completion.err(anyhow!("tmux rejected split-window"));
                    }
                }
            }
            let error = format!("split-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let remote_id = parse_sigil_number(result.output.trim())
            .context("split-window did not return the new pane id")?;
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let mut pending = tmux.inner.pending_splits.lock();
                if let Some(split) = pending
                    .iter_mut()
                    .find(|operation| operation.operation_id == self.operation_id)
                {
                    split.remote_id = Some(remote_id);
                }
                drop(pending);
                if let Some(session_id) = *tmux.inner.tmux_session.lock() {
                    let mut queue = tmux.inner.cmd_queue.lock();
                    if !queue
                        .iter()
                        .any(|command| command.is_full_window_snapshot())
                    {
                        queue.push_back(Box::new(ListAllWindows {
                            session_id,
                            window_id: None,
                        }));
                    }
                    drop(queue);
                    TmuxDomainState::schedule_send_next_command(domain_id);
                }
            }
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let completion = {
                    let mut pending = tmux.inner.pending_splits.lock();
                    pending
                        .iter()
                        .position(|operation| operation.operation_id == self.operation_id)
                        .and_then(|index| pending.remove(index))
                        .map(|operation| operation.completion)
                };
                if let Some(mut completion) = completion {
                    completion.err(anyhow!("tmux split-window timed out"));
                }
            }
        }
        anyhow::bail!("split-window timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct SelectWindow {
    pub window_id: TmuxWindowId,
}

#[derive(Debug)]
pub(crate) struct FocusPane {
    pub operation_id: u64,
    pub pane_id: TmuxPaneId,
    pub window_id: TmuxWindowId,
}

impl TmuxCommand for FocusPane {
    fn guarded_response_count(&self) -> usize {
        2
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "select-window -t @{} ; select-pane -t %{}\n",
            self.window_id, self.pane_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let domain = Mux::get()
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("tmux domain lost"))?;
        let tmux = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("tmux domain lost"))?;
        if result.error {
            let completion = {
                let mut pending = tmux.inner.pending_focus.lock();
                pending
                    .iter()
                    .position(|focus| focus.operation_id == self.operation_id)
                    .and_then(|index| pending.remove(index))
                    .map(|focus| focus.completion)
            };
            if let Some(mut completion) = completion {
                completion.err(anyhow!("tmux rejected pane focus"));
            }
            anyhow::bail!("tmux pane focus in domain={domain_id} failed: {result:#?}");
        }
        if let Some(focus) = tmux
            .inner
            .pending_focus
            .lock()
            .iter_mut()
            .find(|focus| focus.operation_id == self.operation_id)
        {
            focus.accepted = true;
        }
        tmux.inner
            .cmd_queue
            .lock()
            .push_front(Box::new(ListAllPanes {
                window_id: self.window_id,
                prune: false,
                layout_csum: String::new(),
            }));
        TmuxDomainState::schedule_send_next_command(domain_id);
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let completion = {
                    let mut pending = tmux.inner.pending_focus.lock();
                    pending
                        .iter()
                        .position(|focus| focus.operation_id == self.operation_id)
                        .and_then(|index| pending.remove(index))
                        .map(|focus| focus.completion)
                };
                if let Some(mut completion) = completion {
                    completion.err(anyhow!("tmux pane focus timed out"));
                }
            }
        }
        anyhow::bail!("tmux pane focus timed out in domain {domain_id}")
    }
}

#[derive(Debug)]
pub(crate) struct RenameWindow {
    pub operation_id: u64,
    pub window_id: TmuxWindowId,
    pub title: String,
}

impl TmuxCommand for RenameWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let title = self.title.replace(['\r', '\n'], " ");
        format!(
            "rename-window -t @{} {}\n",
            self.window_id,
            shell_words::quote(&title)
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    let completion = {
                        let mut pending = tmux.inner.pending_renames.lock();
                        pending
                            .iter()
                            .position(|rename| rename.operation_id == self.operation_id)
                            .and_then(|index| pending.remove(index))
                            .map(|rename| rename.completion)
                    };
                    if let Some(mut completion) = completion {
                        completion.err(anyhow!("tmux rejected rename-window"));
                    }
                }
            }
            anyhow::bail!("rename-window in domain={domain_id} failed: {result:#?}");
        }
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(rename) = tmux
                    .inner
                    .pending_renames
                    .lock()
                    .iter_mut()
                    .find(|rename| rename.operation_id == self.operation_id)
                {
                    rename.accepted = true;
                }
                if let Some(session_id) = *tmux.inner.tmux_session.lock() {
                    tmux.inner
                        .cmd_queue
                        .lock()
                        .push_front(Box::new(ListAllWindows {
                            session_id,
                            window_id: None,
                        }));
                    TmuxDomainState::schedule_send_next_command(domain_id);
                }
            }
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                let completion = {
                    let mut pending = tmux.inner.pending_renames.lock();
                    pending
                        .iter()
                        .position(|rename| rename.operation_id == self.operation_id)
                        .and_then(|index| pending.remove(index))
                        .map(|rename| rename.completion)
                };
                if let Some(mut completion) = completion {
                    completion.err(anyhow!("tmux rename-window timed out"));
                }
            }
        }
        anyhow::bail!("rename-window timed out in domain {domain_id}")
    }
}

impl TmuxCommand for SelectWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-window -t @{}\n", self.window_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectPane {
    pub pane_id: TmuxPaneId,
}

impl TmuxCommand for SelectPane {
    fn select_pane_id(&self) -> Option<TmuxPaneId> {
        Some(self.pane_id)
    }

    fn is_stale_for_killed_pane(&self, pane_id: TmuxPaneId) -> bool {
        self.pane_id == pane_id
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

// This is a dummy command which indicates the attaching is done, it prevents the tmux output
// the unexpected and unnecessary content when syncing with back end in attaching stage.
#[derive(Debug)]
pub(crate) struct AttachDone;
impl TmuxCommand for AttachDone {
    fn get_command(&self, _domain_id: DomainId) -> String {
        // The command doesn't matter, just give a legal simple command to let process_result called.
        "list-session\n".to_string()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-session in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        // Do nothing, just change the state.
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        *tmux_domain.inner.connection_state.lock() = crate::tab::TmuxConnectionState::Connected;
        for tab_id in tmux_domain
            .inner
            .gui_tabs
            .lock()
            .values()
            .map(|tab| tab.tab_id)
        {
            mux.notify(MuxNotification::TabResized(tab_id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn managed_empty_snapshot_cannot_prune_attached_topology() {
        assert!(should_ignore_empty_managed_snapshot(true, 1, 0));
        assert!(!should_ignore_empty_managed_snapshot(false, 1, 0));
        assert!(!should_ignore_empty_managed_snapshot(true, 0, 0));
        assert!(!should_ignore_empty_managed_snapshot(true, 1, 1));
    }

    #[test]
    fn window_snapshot_delimiter_preserves_spaces_and_quotes_in_names() {
        let items = parse_window_snapshot(
            "$3\x1f@7\x1f80\x1f24\x1f1\x1fwork queue's \"snapshot\"\x1fabcd,80x24,0,0,11\x1f5000\n",
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.session_id, 3);
        assert_eq!(item.window_id, 7);
        assert_eq!(item.window_name, "work queue's \"snapshot\"");
        assert_eq!(item.layout_csum, "abcd");
        assert!(item.window_active);
        assert_eq!(item.history_limit, 5000);
    }

    #[test]
    fn new_window_encodes_command_and_cwd_as_single_tmux_arguments() {
        let encoded = NewWindow {
            command: Some(vec![
                "printf".to_string(),
                "%s\\n".to_string(),
                "hello world".to_string(),
            ]),
            cwd: Some("/tmp/work queue's".to_string()),
        }
        .get_command(0);
        assert_eq!(
            shell_words::split(encoded.trim()).unwrap(),
            [
                "new-window",
                "-P",
                "-F",
                "#{window_id}",
                "-c",
                "/tmp/work queue's",
                "printf '%s\\n' 'hello world'"
            ]
        );
    }

    #[test]
    fn resize_is_one_transport_write_with_two_guarded_responses() {
        let command = resize_command(
            "resize-window -x 80 -y 24 -t @3".to_string(),
            7,
            PtySize {
                rows: 12,
                cols: 39,
                pixel_width: 0,
                pixel_height: 0,
            },
        );
        assert_eq!(
            "resize-window -x 80 -y 24 -t @3 ; resize-pane -x 39 -y 12 -t %7\n",
            command
        );
        assert_eq!(1, command.lines().count());
        let resize = Resize {
            pane_id: 7,
            size: PtySize {
                rows: 12,
                cols: 39,
                pixel_width: 0,
                pixel_height: 0,
            },
        };
        assert_eq!(2, resize.guarded_response_count());
    }

    #[test]
    fn split_requests_stable_remote_pane_id() {
        let command = SplitPane {
            operation_id: 1,
            pane_id: 7,
            direction: SplitDirection::Horizontal,
        }
        .get_command(0);
        assert_eq!("split-window -h -P -F '#{pane_id}' -t %7\n", command);
    }

    #[test]
    fn join_pane_encodes_drop_edge() {
        let cases = [
            (
                SplitDirection::Horizontal,
                false,
                "join-pane -h -b -p 50 -s %11 -t %22\n",
            ),
            (
                SplitDirection::Horizontal,
                true,
                "join-pane -h -p 50 -s %11 -t %22\n",
            ),
            (
                SplitDirection::Vertical,
                false,
                "join-pane -v -b -p 50 -s %11 -t %22\n",
            ),
            (
                SplitDirection::Vertical,
                true,
                "join-pane -v -p 50 -s %11 -t %22\n",
            ),
        ];

        for (direction, target_is_second, expected) in cases {
            let command = JoinPane {
                operation_id: 9,
                source: 11,
                source_window: 3,
                target: 22,
                request: SplitRequest {
                    direction,
                    target_is_second,
                    top_level: false,
                    size: SplitSize::Percent(50),
                },
            };
            assert_eq!(command.get_command(0), expected);
        }
    }

    #[test]
    fn break_pane_requests_the_new_stable_window_id() {
        let command = BreakPane {
            operation_id: 3,
            source: 11,
        };
        assert_eq!(
            command.get_command(0),
            "break-pane -d -P -F '#{window_id}' -s %11\n"
        );
    }

    #[test]
    fn focus_pane_is_one_write_with_two_guarded_responses() {
        let command = FocusPane {
            operation_id: 4,
            pane_id: 11,
            window_id: 7,
        };
        assert_eq!(
            command.get_command(0),
            "select-window -t @7 ; select-pane -t %11\n"
        );
        assert_eq!(command.guarded_response_count(), 2);
    }

    #[test]
    fn reposition_completes_only_after_accepted_target_ownership_snapshot() {
        let target_ownership = HashMap::from([(11, 4), (22, 4)]);
        assert!(reposition_is_reconciled(true, 11, 22, 4, &target_ownership));
        assert!(!reposition_is_reconciled(
            false,
            11,
            22,
            4,
            &target_ownership
        ));
        assert!(!reposition_is_reconciled(
            true,
            11,
            22,
            5,
            &target_ownership
        ));
        assert!(!reposition_is_reconciled(
            true,
            11,
            33,
            4,
            &target_ownership
        ));
    }

    #[test]
    fn topology_plan_classifies_stable_pane_ownership() -> anyhow::Result<()> {
        let current = HashMap::from([
            (
                1,
                parse_layout_tree("80x24,0,0{39x24,0,0,10,40x24,40,0,11}")?,
            ),
            (2, parse_layout_tree("80x24,0,0,20")?),
            (3, parse_layout_tree("80x24,0,0,30")?),
        ]);
        let snapshot = HashMap::from([
            (
                1,
                parse_layout_tree("120x40,0,0{59x40,0,0,10,60x40,60,0,12}")?,
            ),
            (2, parse_layout_tree("80x24,0,0,30")?),
            (4, parse_layout_tree("80x24,0,0,40")?),
        ]);

        let plan = plan_topology_reconciliation(&current, &snapshot)?;
        assert_eq!(
            plan.retained,
            HashSet::from([PaneOwnership {
                pane_id: 10,
                window_id: 1,
            }])
        );
        assert_eq!(
            plan.created,
            HashSet::from([
                PaneOwnership {
                    pane_id: 12,
                    window_id: 1,
                },
                PaneOwnership {
                    pane_id: 40,
                    window_id: 4,
                },
            ])
        );
        assert_eq!(
            plan.removed,
            HashSet::from([
                PaneOwnership {
                    pane_id: 11,
                    window_id: 1,
                },
                PaneOwnership {
                    pane_id: 20,
                    window_id: 2,
                },
            ])
        );
        assert_eq!(
            plan.moved,
            HashSet::from([(
                PaneOwnership {
                    pane_id: 30,
                    window_id: 3,
                },
                PaneOwnership {
                    pane_id: 30,
                    window_id: 2,
                },
            )])
        );
        assert!(plan.unchanged_windows.is_empty());
        Ok(())
    }

    #[test]
    fn topology_plan_preserves_cell_resized_window() -> anyhow::Result<()> {
        let current = HashMap::from([(
            1,
            parse_layout_tree("80x24,0,0{39x24,0,0,10,40x24,40,0,11}")?,
        )]);
        let snapshot = HashMap::from([(
            1,
            parse_layout_tree("120x40,0,0{59x40,0,0,10,60x40,60,0,11}")?,
        )]);
        let plan = plan_topology_reconciliation(&current, &snapshot)?;
        assert_eq!(plan.retained.len(), 2);
        assert_eq!(plan.unchanged_windows, HashSet::from([1]));
        Ok(())
    }

    #[test]
    fn topology_plan_matches_surviving_nested_subtree() -> anyhow::Result<()> {
        let current = HashMap::from([(
            1,
            parse_layout_tree("120x40,0,0{59x40,0,0[59x19,0,0,10,59x20,0,20,11],60x40,60,0,12}")?,
        )]);
        let snapshot = HashMap::from([(
            1,
            parse_layout_tree(
                "180x40,0,0{39x40,0,0,13,59x40,40,0[59x19,40,0,10,59x20,40,20,11],80x40,100,0,12}",
            )?,
        )]);
        let plan = plan_topology_reconciliation(&current, &snapshot)?;
        let preserved = parse_layout_tree("59x40,40,0[59x19,40,0,10,59x20,40,20,11]")?.topology();
        assert!(plan.preserved_subtrees.contains(&(1, preserved)));
        assert!(!plan.unchanged_windows.contains(&1));
        Ok(())
    }

    #[test]
    fn topology_plan_rejects_duplicate_stable_pane_ids() -> anyhow::Result<()> {
        let snapshot = HashMap::from([
            (1, parse_layout_tree("80x24,0,0,10")?),
            (2, parse_layout_tree("80x24,0,0,10")?),
        ]);
        let error = plan_topology_reconciliation(&HashMap::new(), &snapshot).unwrap_err();
        assert!(error.to_string().contains("appears in both"));
        Ok(())
    }

    #[test]
    fn snapshot_geometry_seeds_pixel_ratio_without_cell_feedback() -> anyhow::Result<()> {
        let layout = parse_layout_tree("120x40,0,0{29x40,0,0,1,90x40,30,0,2}")?;
        let LayoutNode::SplitHorizontal { geometry, children } = layout else {
            panic!("expected horizontal split");
        };
        let first = terminal_size_for_geometry(
            layout_geometry(&children[0]),
            geometry,
            TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 1200,
                pixel_height: 800,
                dpi: 144,
            },
        );
        assert_eq!(first.cols, 29);
        assert_eq!(first.pixel_width, 290);
        assert_eq!(first.pixel_height, 800);
        assert_eq!(first.dpi, 144);
        Ok(())
    }

    #[test]
    fn rename_window_quotes_title() {
        let command = RenameWindow {
            operation_id: 1,
            window_id: 7,
            title: "work's\nqueue".to_string(),
        };
        assert_eq!(
            command.get_command(0),
            "rename-window -t @7 'work'\\''s queue'\n"
        );
    }
}

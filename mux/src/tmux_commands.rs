use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::{PaneId, alloc_pane_id};
use crate::tab::{
    SplitDirection, SplitDirectionAndSize, SplitRequest, SplitSize, Tab, TabId, Tree,
};
use crate::tmux::{AttachState, TmuxDomain, TmuxDomainState, TmuxRemotePane, TmuxTab};
use crate::tmux_pty::{TmuxChild, TmuxPty};
use crate::{Mux, MuxNotification, Pane};
use anyhow::{Context, anyhow};
use parking_lot::{Condvar, Mutex};
use portable_pty::{MasterPty, PtySize};
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::Arc;
use termwiz::escape::csi::{CSI, Cursor};
use termwiz::escape::{Action, OneBased};
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

pub(crate) trait TmuxCommand: Send + Debug {
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        anyhow::bail!("tmux command timed out in domain {domain_id}: {self:?}")
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
            size,
        ),
        LayoutNode::SplitVertical { geometry, children } => build_snapshot_split(
            children,
            SplitDirection::Vertical,
            *geometry,
            panes,
            reusable,
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
    size: TerminalSize,
) -> anyhow::Result<Tree> {
    if children.len() < 2 {
        anyhow::bail!("tmux split must contain at least two children");
    }
    let first_geometry = layout_geometry(&children[0]);
    let remaining_geometry = union_layout_geometry(&children[1..])?;
    let first_size = terminal_size_for_geometry(first_geometry, geometry, size);
    let remaining_size = terminal_size_for_geometry(remaining_geometry, geometry, size);
    let left = build_snapshot_tree(&children[0], panes, reusable, first_size)?;
    let right = if children.len() == 2 {
        build_snapshot_tree(&children[1], panes, reusable, remaining_size)?
    } else {
        build_snapshot_split(
            &children[1..],
            direction,
            remaining_geometry,
            panes,
            reusable,
            remaining_size,
        )?
    };
    Ok(Tree::Node {
        left: Box::new(left),
        right: Box::new(right),
        data: Some(SplitDirectionAndSize::from_snapshot(
            direction,
            first_size,
            remaining_size,
        )),
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
            mux.remove_pane(local_pane_id);
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

        let mux = Mux::get();
        mux.remove_tab(tab.tab_id);
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
        let local_pane_id = alloc_pane_id();
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

        {
            let mut pane_map = self.remote_panes.lock();
            pane_map.insert(pane.pane_id, ref_pane.clone());
        }

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: self.cmd_queue.clone(),
            master_pane: ref_pane,
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
            domain_id: self.domain_id,
            pane_id: pane.pane_id,
            cmd_queue: self.cmd_queue.clone(),
        };

        let terminal = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );

        Ok(Arc::new(LocalPane::new(
            local_pane_id,
            terminal,
            Box::new(child),
            Box::new(pane_pty),
            Box::new(writer),
            self.domain_id,
            "tmux pane".to_string(),
        )))
    }

    pub fn split_pane(
        &self,
        tab_id: TabId,
        pane_id: PaneId,
        remote_id: TmuxPaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mux = Mux::get();
        let tab = match mux.get_tab(tab_id) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", tab_id),
        };

        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let window_id = match self.gui_tabs.lock().iter().find(|t| t.1.tab_id == tab_id) {
            Some((_, tab)) => tab.tmux_window_id,
            None => anyhow::bail!("No tab {}", tab_id),
        };

        let p = PaneItem {
            session_id: 0,
            window_id: window_id,
            pane_id: remote_id,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: split_size.second.cols as u64,
            pane_height: split_size.second.rows as u64,
            pane_left: 0,
            pane_top: 0,
            pane_active: false,
        };

        let pane = self.create_pane(&p).context("failed to create pane")?;
        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))?;

        self.add_attached_pane(window_id, remote_id)?;

        let _ = mux.add_pane(&pane);

        return Ok(pane);
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
                            tab.set_active_pane(&local_pane);
                            mux.notify(MuxNotification::PaneFocused(local_pane.pane_id()));
                        }
                        None => {}
                    }
                }
            }

            log::info!("new pane synced, id: {}", pane.pane_id);
        }

        Ok(())
    }

    fn sync_window_state(&self, windows: &[WindowItem], new_window: bool) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = Mux::get();

        let current_layouts = self
            .gui_tabs
            .lock()
            .iter()
            .map(|(window_id, tab)| (*window_id, tab.layout_tree.clone()))
            .collect();
        let snapshot_layouts = windows
            .iter()
            .filter(|window| window.session_id == current_session)
            .map(|window| (window.window_id, window.layout_tree.clone()))
            .collect();
        let topology_plan = plan_topology_reconciliation(&current_layouts, &snapshot_layouts)?;
        log::debug!("tmux topology reconciliation plan: {topology_plan:#?}");

        let remote_by_local: HashMap<_, _> = self
            .remote_panes
            .lock()
            .iter()
            .map(|(remote_id, pane)| (pane.lock().local_pane_id, *remote_id))
            .collect();
        let mut reusable_subtrees = HashMap::new();
        for (window_id, attached) in self.gui_tabs.lock().iter() {
            let Some(tab) = mux.get_tab(attached.tab_id) else {
                continue;
            };
            let mut local_subtrees = HashMap::new();
            collect_reusable_local_subtrees(
                &tab.snapshot_pane_tree(),
                &remote_by_local,
                &mut local_subtrees,
            )?;
            for topology in topology_plan.preserved_subtrees.iter().filter_map(
                |(preserved_window, topology)| {
                    (*preserved_window == *window_id).then_some(topology)
                },
            ) {
                if let Some(tree) = local_subtrees.remove(topology) {
                    reusable_subtrees.insert((*window_id, topology.clone()), tree);
                }
            }
        }
        log::debug!(
            "tmux reconciliation retained {} local split subtrees",
            reusable_subtrees.len()
        );

        let pane_objects: HashMap<_, _> = self
            .remote_panes
            .lock()
            .iter()
            .filter_map(|(remote_id, pane)| {
                mux.get_pane(pane.lock().local_pane_id)
                    .map(|pane| (*remote_id, pane))
            })
            .collect();
        let mut candidate_trees = HashMap::new();
        for window in windows
            .iter()
            .filter(|window| window.session_id == current_session)
        {
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
            if pane_ids
                .iter()
                .any(|pane_id| !pane_objects.contains_key(pane_id))
            {
                continue;
            }
            let reusable = reusable_subtrees
                .iter()
                .filter_map(|((reusable_window, topology), tree)| {
                    (*reusable_window == window.window_id)
                        .then(|| (topology.clone(), clone_pane_tree(tree)))
                })
                .collect();
            candidate_trees.insert(
                window.window_id,
                build_snapshot_tree(
                    &window.layout_tree,
                    &pane_objects,
                    &reusable,
                    tab.get_size(),
                )?,
            );
        }
        log::debug!(
            "tmux reconciliation built {} complete candidate trees",
            candidate_trees.len()
        );

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
                        let local_pane = self.create_pane(&p).context("failed to create pane")?;
                        tab.assign_pane(&local_pane);
                        local_pane.resize(size)?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
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
                        local_pane = self.create_pane(&p).context("failed to create pane")?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
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
                if let Some(mut pending) = self.pending_new_tabs.lock().pop_front() {
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
                            tmux_domain
                                .inner
                                .cmd_queue
                                .lock()
                                .push_back(Box::new(SelectPane { pane_id: pane_id }));
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

impl TmuxCommand for ListAllWindows {
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
        let mut items = vec![];

        for line in result.output.split('\n') {
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
                .parse::<usize>()?;

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

            let window_active = window_active == 1;

            if let Some(x) = self.window_id {
                if x != window_id {
                    continue;
                }
            }

            let layout_csum = window_layout
                .get(0..4)
                .ok_or_else(|| anyhow!("missing window_layout"))?;
            let window_layout = window_layout
                .get(5..)
                .ok_or_else(|| anyhow!("missing window_layout"))?;

            let layout = parse_layout(window_layout)?;
            let layout_tree = parse_layout_tree(window_layout)?;

            items.push(WindowItem {
                session_id,
                window_id,
                window_width,
                window_height,
                window_active,
                window_name: window_name.to_string(),
                layout,
                layout_tree,
                layout_csum: layout_csum.to_string(),
                history_limit,
            });
        }

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

impl TmuxCommand for Resize {
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

        if let Some(_x) = support_commands.get("resize-window") {
            format!(
                "resize-window -x {} -y {} -t @{}\nresize-pane -x {} -y {} -t %{}\n",
                size.cols, size.rows, tmux_window_id, self.size.cols, self.size.rows, self.pane_id
            )
        } else if let Some(x) = support_commands.get("refresh-client") {
            if x.contains("-C XxY") {
                format!(
                    "refresh-client -C {}x{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols, size.rows, self.size.cols, self.size.rows, self.pane_id
                )
            } else {
                format!(
                    "refresh-client -C {},{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols, size.rows, self.size.cols, self.size.rows, self.pane_id
                )
            }
        } else {
            log::info!("The tmux version is not supported");
            return "".to_string();
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("resize-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
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

        let pane_map = tmux_domain.inner.remote_panes.lock();
        if let Some(pane) = pane_map.get(&self.pane_id) {
            let mut pane = pane.lock();
            if let Some(p) = mux.get_pane(pane.local_pane_id) {
                tmux_domain.inner.set_pane_cursor_position(&p, 0, 0);
            }

            pane.output_write
                .write_all(unescaped.as_bytes())
                .context("writing capture pane result to output")?;
        }

        Ok(())
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
pub(crate) struct NewWindow;
impl TmuxCommand for NewWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        "new-window\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    if let Some(mut pending) = tmux.inner.pending_new_tabs.lock().pop_front() {
                        pending.err(anyhow!("tmux rejected new-window"));
                    }
                }
            }
            let error = format!("new-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(mut pending) = tmux.inner.pending_new_tabs.lock().pop_front() {
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
            anyhow::bail!("kill-window in domain={domain_id} failed: {result:#?}");
        }
        Ok(())
    }
}

impl TmuxCommand for KillPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("kill-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            anyhow::bail!("kill-pane in domain={domain_id} failed: {result:#?}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct JoinPane {
    pub pane_id: PaneId,
    pub target_pane_id: PaneId,
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
            tmux.inner.pane_reposition_failed(self.source_window);
            anyhow::bail!("join-pane in domain={domain_id} failed: {result:#?}");
        }
        tmux.inner.pane_repositioned(
            self.pane_id,
            self.target_pane_id,
            self.request,
            self.source_window,
        )
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                tmux.inner.pane_reposition_failed(self.source_window);
            }
        }
        anyhow::bail!("join-pane timed out in domain {domain_id}")
    }
}

impl TmuxCommand for SplitPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        if self.direction == SplitDirection::Horizontal {
            format!("split-window -h -t %{}\n", self.pane_id)
        } else {
            format!("split-window -v -t %{}\n", self.pane_id)
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            if let Some(domain) = Mux::get().get_domain(domain_id) {
                if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                    if let Some(mut pending) = tmux.inner.pending_splits.lock().pop_front() {
                        pending.err(anyhow!("tmux rejected split-window"));
                    }
                }
            }
            let error = format!("split-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn process_timeout(&self, domain_id: DomainId) -> anyhow::Result<()> {
        if let Some(domain) = Mux::get().get_domain(domain_id) {
            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                if let Some(mut pending) = tmux.inner.pending_splits.lock().pop_front() {
                    pending.err(anyhow!("tmux split-window timed out"));
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
pub(crate) struct RenameWindow {
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
            anyhow::bail!("rename-window in domain={domain_id} failed: {result:#?}");
        }
        Ok(())
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
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

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
                pane_id: 1,
                target_pane_id: 2,
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
            window_id: 7,
            title: "work's\nqueue".to_string(),
        };
        assert_eq!(
            command.get_command(0),
            "rename-window -t @7 'work'\\''s queue'\n"
        );
    }
}

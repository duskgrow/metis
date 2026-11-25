//! Directional focus over semantic regions, independent of rendering and GPUI.

use super::history::{FocusAnchor, MainView, Region};

/// The Shell always has horizontal Navigation/Main siblings. `regions` are the
/// ordered vertical siblings inside Main; item-level controls stay item IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FocusTopology {
    view: MainView,
    regions: Vec<Region>,
    approval: Option<String>,
}

impl FocusTopology {
    pub(super) fn new(view: MainView, approval: Option<String>, has_queue: bool) -> Self {
        let approval = match view {
            MainView::Session(_) => approval,
            MainView::Diff { .. } | MainView::Subagent { .. } => None,
        };
        let regions = match &view {
            MainView::Session(_) => {
                let mut regions = vec![Region::Header, Region::Timeline];
                if approval.is_some() {
                    regions.push(Region::Approval);
                }
                if has_queue {
                    regions.push(Region::Queue);
                }
                regions.push(Region::Composer);
                regions
            }
            MainView::Diff { .. } => vec![Region::SourceHeader, Region::DiffContent],
            MainView::Subagent { .. } => vec![Region::SourceHeader, Region::Transcript],
        };
        Self {
            view,
            regions,
            approval,
        }
    }

    pub(super) fn view(&self) -> &MainView {
        &self.view
    }

    pub(super) fn regions(&self) -> &[Region] {
        &self.regions
    }

    fn contains(&self, region: Region) -> bool {
        self.regions.contains(&region)
    }
}

/// Directional entry points remain pure in this slice; no global keymap is
/// installed for them.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FocusMove {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FocusScope {
    Shell,
    Overlay(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FocusTarget {
    pub(super) scope: FocusScope,
    pub(super) anchor: FocusAnchor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FocusIntent {
    Focus(FocusTarget),
    CloseOverlay {
        overlay_id: String,
        restore: FocusTarget,
    },
    Noop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OverlayEntry {
    id: String,
    trigger: FocusTarget,
    anchor: FocusAnchor,
}

/// Pure local focus state. Moving focus cannot select a Session or mutate facts.
#[derive(Clone, Debug)]
pub(super) struct FocusReducer {
    topology: FocusTopology,
    shell: FocusAnchor,
    last_active: Option<FocusAnchor>,
    last_navigation: FocusAnchor,
    overlays: Vec<OverlayEntry>,
}

impl FocusReducer {
    pub(super) fn new(topology: FocusTopology) -> Self {
        let navigation = FocusAnchor::new(topology.view.clone(), Region::Navigation);
        Self {
            topology,
            shell: navigation.clone(),
            last_active: None,
            last_navigation: navigation,
            overlays: Vec::new(),
        }
    }

    pub(super) fn current(&self) -> FocusTarget {
        self.overlays.last().map_or_else(
            || FocusTarget {
                scope: FocusScope::Shell,
                anchor: self.shell.clone(),
            },
            |overlay| FocusTarget {
                scope: FocusScope::Overlay(overlay.id.clone()),
                anchor: overlay.anchor.clone(),
            },
        )
    }

    #[cfg(test)]
    pub(super) fn shell_anchor(&self) -> &FocusAnchor {
        &self.shell
    }

    #[cfg(test)]
    pub(super) fn overlay_depth(&self) -> usize {
        self.overlays.len()
    }

    #[cfg(test)]
    pub(super) fn reduce(&mut self, movement: FocusMove) -> FocusIntent {
        if !self.overlays.is_empty() {
            return FocusIntent::Noop;
        }

        let next = match (movement, self.shell.region) {
            (FocusMove::Right, Region::Navigation) => self.last_active.clone().map_or_else(
                || default_main_anchor(&self.topology),
                |anchor| normalize_anchor(&self.topology, anchor),
            ),
            (FocusMove::Left | FocusMove::Up | FocusMove::Down, Region::Navigation)
            | (FocusMove::Right, _) => return FocusIntent::Noop,
            (FocusMove::Left, _) => self.last_navigation.clone(),
            (FocusMove::Up, _) => return self.vertical_move(FocusMove::Up),
            (FocusMove::Down, _) => return self.vertical_move(FocusMove::Down),
        };
        self.move_shell(next)
    }

    /// Rebuild after content or layout changes. Existing anchors survive when
    /// their region remains visible; content appearing alone never steals focus.
    pub(super) fn rebuild(&mut self, topology: FocusTopology) -> FocusIntent {
        let previous = self.shell.clone();
        self.topology = topology;
        self.shell = normalize_anchor(&self.topology, previous.clone());
        self.last_active = self
            .last_active
            .take()
            .map(|anchor| normalize_anchor(&self.topology, anchor));
        if self.last_navigation.view != self.topology.view {
            self.last_navigation = FocusAnchor::new(self.topology.view.clone(), Region::Navigation);
        }
        for overlay in &mut self.overlays {
            if overlay.trigger.scope == FocusScope::Shell {
                overlay.trigger.anchor =
                    normalize_anchor(&self.topology, overlay.trigger.anchor.clone());
            }
        }

        if self.overlays.is_empty() && previous != self.shell {
            self.shell_intent()
        } else {
            FocusIntent::Noop
        }
    }

    /// Apply the `FocusAnchor` carried by source return or Back/Forward history.
    pub(super) fn restore(&mut self, topology: FocusTopology, anchor: FocusAnchor) -> FocusIntent {
        self.topology = topology;
        self.overlays.clear();
        let anchor = normalize_anchor(&self.topology, anchor);
        if self.last_navigation.view != self.topology.view {
            self.last_navigation = FocusAnchor::new(self.topology.view.clone(), Region::Navigation);
        }
        self.last_active = if anchor.region == Region::Navigation {
            self.last_active
                .take()
                .filter(|last| last.view == self.topology.view)
                .map(|last| normalize_anchor(&self.topology, last))
        } else {
            Some(anchor.clone())
        };
        if anchor.region == Region::Navigation {
            self.last_navigation = anchor.clone();
        }
        self.shell = anchor;
        self.shell_intent()
    }

    pub(super) fn open_overlay(
        &mut self,
        overlay_id: impl Into<String>,
        initial_item_id: Option<String>,
    ) -> FocusIntent {
        let trigger = self.current();
        let mut anchor = trigger.anchor.clone();
        anchor.item_id = initial_item_id;
        let overlay_id = overlay_id.into();
        self.overlays.push(OverlayEntry {
            id: overlay_id.clone(),
            trigger,
            anchor: anchor.clone(),
        });
        FocusIntent::Focus(FocusTarget {
            scope: FocusScope::Overlay(overlay_id),
            anchor,
        })
    }

    /// Escape dismisses only the top scope and restores its logical trigger.
    pub(super) fn escape(&mut self) -> FocusIntent {
        let Some(overlay) = self.overlays.pop() else {
            return FocusIntent::Noop;
        };
        FocusIntent::CloseOverlay {
            overlay_id: overlay.id,
            restore: overlay.trigger,
        }
    }

    #[cfg(test)]
    fn vertical_move(&mut self, movement: FocusMove) -> FocusIntent {
        let Some(index) = self
            .topology
            .regions
            .iter()
            .position(|region| *region == self.shell.region)
        else {
            return FocusIntent::Noop;
        };
        let target = match movement {
            FocusMove::Up => index.checked_sub(1),
            FocusMove::Down => {
                let next = index + 1;
                (next < self.topology.regions.len()).then_some(next)
            }
            FocusMove::Left | FocusMove::Right => None,
        };
        let Some(target) = target else {
            return FocusIntent::Noop;
        };
        let region = self.topology.regions[target];
        let item_id = match region {
            Region::Approval => self.topology.approval.clone(),
            Region::Timeline if self.shell.region == Region::Approval => self.shell.item_id.clone(),
            _ => None,
        };
        self.move_shell(FocusAnchor {
            view: self.topology.view.clone(),
            region,
            item_id,
        })
    }

    #[cfg(test)]
    fn move_shell(&mut self, next: FocusAnchor) -> FocusIntent {
        if next == self.shell {
            return FocusIntent::Noop;
        }
        let previous = std::mem::replace(&mut self.shell, next);
        if previous.region == Region::Navigation {
            self.last_navigation = previous;
        } else {
            self.last_active = Some(previous);
        }
        if self.shell.region == Region::Navigation {
            self.last_navigation = self.shell.clone();
        } else {
            self.last_active = Some(self.shell.clone());
        }
        self.shell_intent()
    }

    fn shell_intent(&self) -> FocusIntent {
        FocusIntent::Focus(FocusTarget {
            scope: FocusScope::Shell,
            anchor: self.shell.clone(),
        })
    }
}

fn default_main_anchor(topology: &FocusTopology) -> FocusAnchor {
    match &topology.view {
        MainView::Session(_) => topology.approval.as_ref().map_or_else(
            || FocusAnchor::new(topology.view.clone(), Region::Composer),
            |call_id| {
                FocusAnchor::new(topology.view.clone(), Region::Approval).with_item(call_id.clone())
            },
        ),
        MainView::Diff { .. } | MainView::Subagent { .. } => {
            FocusAnchor::new(topology.view.clone(), Region::SourceHeader)
        }
    }
}

fn normalize_anchor(topology: &FocusTopology, mut anchor: FocusAnchor) -> FocusAnchor {
    if anchor.view != topology.view {
        return default_main_anchor(topology);
    }
    if anchor.region == Region::Navigation {
        return anchor;
    }

    if matches!(topology.view, MainView::Session(_)) {
        match anchor.region {
            Region::Approval => {
                if let Some(call_id) = &topology.approval
                    && anchor
                        .item_id
                        .as_ref()
                        .is_none_or(|current| current == call_id)
                {
                    anchor.item_id = Some(call_id.clone());
                    return anchor;
                }
                return FocusAnchor {
                    view: topology.view.clone(),
                    region: Region::Timeline,
                    item_id: anchor.item_id,
                };
            }
            Region::Queue if !topology.contains(Region::Queue) => {
                return FocusAnchor::new(topology.view.clone(), Region::Composer);
            }
            _ => {}
        }
    }
    if topology.contains(anchor.region) {
        return anchor;
    }

    match topology.view {
        MainView::Session(_) => FocusAnchor::new(topology.view.clone(), Region::Composer),
        MainView::Diff { .. } | MainView::Subagent { .. } => {
            FocusAnchor::new(topology.view.clone(), Region::SourceHeader)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbench::history::{DIFF_ENTRY_ITEM_ID, NavigationSnapshot, NavigationState};
    use crate::workbench::identity::{ProjectKey, SessionKey, SubagentKey, WorktreeKey};

    fn worktree(id: &str) -> WorktreeKey {
        WorktreeKey::new(ProjectKey::new("host", "project"), id)
    }

    fn session(id: &str) -> SessionKey {
        SessionKey::new(worktree("main"), id)
    }

    fn session_view(id: &str) -> MainView {
        MainView::Session(session(id))
    }

    fn diff_view() -> MainView {
        let origin = session("origin");
        MainView::Diff {
            worktree: origin.worktree.clone(),
            origin,
        }
    }

    fn subagent_view() -> MainView {
        MainView::Subagent {
            key: SubagentKey::new(session("parent"), "child"),
        }
    }

    fn expect_focus(intent: FocusIntent) -> FocusTarget {
        match intent {
            FocusIntent::Focus(target) => target,
            other => panic!("expected focus intent, got {other:?}"),
        }
    }

    fn assert_shell(anchor: &FocusAnchor, region: Region, item_id: Option<&str>) {
        assert_eq!(anchor.region, region);
        assert_eq!(anchor.item_id.as_deref(), item_id);
    }

    #[test]
    fn session_without_optional_regions_skips_them_and_never_wraps() {
        let topology = FocusTopology::new(session_view("session"), None, false);
        assert_eq!(
            topology.regions(),
            &[Region::Header, Region::Timeline, Region::Composer]
        );
        let mut focus = FocusReducer::new(topology);

        let target = expect_focus(focus.reduce(FocusMove::Right));
        assert_eq!(target.scope, FocusScope::Shell);
        assert_shell(&target.anchor, Region::Composer, None);
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Up)).anchor,
            Region::Timeline,
            None,
        );
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Up)).anchor,
            Region::Header,
            None,
        );
        assert_eq!(focus.reduce(FocusMove::Up), FocusIntent::Noop);
        assert_eq!(focus.reduce(FocusMove::Right), FocusIntent::Noop);
    }

    #[test]
    fn session_orders_approval_and_queue_and_uses_stable_tool_identity() {
        let topology = FocusTopology::new(session_view("session"), Some("call-1".to_owned()), true);
        assert_eq!(
            topology.regions(),
            &[
                Region::Header,
                Region::Timeline,
                Region::Approval,
                Region::Queue,
                Region::Composer,
            ]
        );
        let mut focus = FocusReducer::new(topology);

        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Right)).anchor,
            Region::Approval,
            Some("call-1"),
        );
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Down)).anchor,
            Region::Queue,
            None,
        );
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Down)).anchor,
            Region::Composer,
            None,
        );
        assert_eq!(focus.reduce(FocusMove::Down), FocusIntent::Noop);
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Up)).anchor,
            Region::Queue,
            None,
        );
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Up)).anchor,
            Region::Approval,
            Some("call-1"),
        );
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Up)).anchor,
            Region::Timeline,
            Some("call-1"),
        );
    }

    #[test]
    fn diff_and_subagent_have_only_source_and_content_regions() {
        for (view, content, regions) in [
            (
                diff_view(),
                Region::DiffContent,
                &[Region::SourceHeader, Region::DiffContent][..],
            ),
            (
                subagent_view(),
                Region::Transcript,
                &[Region::SourceHeader, Region::Transcript][..],
            ),
        ] {
            let topology = FocusTopology::new(view, None, false);
            assert_eq!(topology.regions(), regions);
            let mut focus = FocusReducer::new(topology);
            assert_shell(
                &expect_focus(focus.reduce(FocusMove::Right)).anchor,
                Region::SourceHeader,
                None,
            );
            assert_shell(
                &expect_focus(focus.reduce(FocusMove::Down)).anchor,
                content,
                None,
            );
            assert_eq!(focus.reduce(FocusMove::Down), FocusIntent::Noop);
            assert_shell(
                &expect_focus(focus.reduce(FocusMove::Left)).anchor,
                Region::Navigation,
                None,
            );
            assert_shell(
                &expect_focus(focus.reduce(FocusMove::Right)).anchor,
                content,
                None,
            );
        }
    }

    #[test]
    fn navigation_restores_last_active_without_changing_session_selection() {
        let selected = session("selected");
        let view = MainView::Session(selected.clone());
        let topology = FocusTopology::new(view.clone(), None, false);
        let navigation = NavigationState::new(selected);
        let mut focus = FocusReducer::new(topology.clone());
        let history_anchor = FocusAnchor::new(view, Region::Timeline).with_item("message-9");
        expect_focus(focus.restore(topology.clone(), history_anchor.clone()));

        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Left)).anchor,
            Region::Navigation,
            None,
        );
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Right)).anchor,
            Region::Timeline,
            Some("message-9"),
        );
        assert_eq!(focus.shell_anchor(), &history_anchor);
        assert_eq!(
            navigation.current_view(),
            &MainView::Session(session("selected"))
        );
    }

    #[test]
    fn background_approval_updates_topology_without_stealing_focus() {
        let view = session_view("session");
        let mut focus = FocusReducer::new(FocusTopology::new(view.clone(), None, false));
        expect_focus(focus.reduce(FocusMove::Right));
        assert_shell(focus.shell_anchor(), Region::Composer, None);

        let with_approval = FocusTopology::new(view.clone(), Some("call-1".to_owned()), true);
        assert_eq!(focus.rebuild(with_approval.clone()), FocusIntent::Noop);
        assert_shell(focus.shell_anchor(), Region::Composer, None);

        expect_focus(focus.reduce(FocusMove::Left));
        assert_eq!(focus.rebuild(with_approval), FocusIntent::Noop);
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Right)).anchor,
            Region::Composer,
            None,
        );

        let first_visit =
            FocusReducer::new(FocusTopology::new(view, Some("call-2".to_owned()), false));
        assert_shell(first_visit.shell_anchor(), Region::Navigation, None);
    }

    #[test]
    fn disappeared_approval_uses_tool_summary_and_queue_uses_composer() {
        let view = session_view("session");
        let with_both = FocusTopology::new(view.clone(), Some("call-1".to_owned()), true);
        let without_both = FocusTopology::new(view, None, false);
        let mut focus = FocusReducer::new(with_both.clone());
        expect_focus(focus.reduce(FocusMove::Right));

        let target = expect_focus(focus.rebuild(without_both.clone()));
        assert_eq!(target.scope, FocusScope::Shell);
        assert_shell(&target.anchor, Region::Timeline, Some("call-1"));

        let queue_anchor =
            FocusAnchor::new(with_both.view.clone(), Region::Queue).with_item("queue-1");
        expect_focus(focus.restore(with_both, queue_anchor));
        let target = expect_focus(focus.rebuild(without_both));
        assert_shell(&target.anchor, Region::Composer, None);
    }

    #[test]
    fn hidden_last_active_is_repaired_before_navigation_returns_to_main() {
        let view = session_view("session");
        let with_approval = FocusTopology::new(view.clone(), Some("call-1".to_owned()), false);
        let mut focus = FocusReducer::new(with_approval);
        expect_focus(focus.reduce(FocusMove::Right));
        expect_focus(focus.reduce(FocusMove::Left));

        assert_eq!(
            focus.rebuild(FocusTopology::new(view, None, false)),
            FocusIntent::Noop
        );
        assert_shell(focus.shell_anchor(), Region::Navigation, None);
        assert_shell(
            &expect_focus(focus.reduce(FocusMove::Right)).anchor,
            Region::Timeline,
            Some("call-1"),
        );
    }

    #[test]
    fn overlays_pause_direction_and_escape_unwinds_one_scope_at_a_time() {
        let topology = FocusTopology::new(session_view("session"), None, false);
        let mut focus = FocusReducer::new(topology);
        expect_focus(focus.reduce(FocusMove::Right));
        let shell_trigger = focus.current();

        let model = expect_focus(focus.open_overlay("model", Some("model-a".to_owned())));
        assert_eq!(model.scope, FocusScope::Overlay("model".to_owned()));
        assert_shell(&model.anchor, Region::Composer, Some("model-a"));
        assert_eq!(focus.reduce(FocusMove::Up), FocusIntent::Noop);
        assert_shell(focus.shell_anchor(), Region::Composer, None);

        let effort = expect_focus(focus.open_overlay("effort", Some("high".to_owned())));
        assert_eq!(effort.scope, FocusScope::Overlay("effort".to_owned()));
        assert_eq!(focus.overlay_depth(), 2);
        assert_eq!(
            focus.escape(),
            FocusIntent::CloseOverlay {
                overlay_id: "effort".to_owned(),
                restore: FocusTarget {
                    scope: FocusScope::Overlay("model".to_owned()),
                    anchor: model.anchor,
                },
            }
        );
        assert_eq!(focus.overlay_depth(), 1);
        assert_eq!(focus.reduce(FocusMove::Left), FocusIntent::Noop);
        assert_eq!(
            focus.escape(),
            FocusIntent::CloseOverlay {
                overlay_id: "model".to_owned(),
                restore: shell_trigger,
            }
        );
        assert_eq!(focus.overlay_depth(), 0);
        assert_eq!(focus.escape(), FocusIntent::Noop);
    }

    #[test]
    fn rebuilding_under_overlay_preserves_scope_and_repairs_return_anchor() {
        let view = session_view("session");
        let with_approval = FocusTopology::new(view.clone(), Some("call-1".to_owned()), false);
        let mut focus = FocusReducer::new(with_approval);
        expect_focus(focus.reduce(FocusMove::Right));
        expect_focus(focus.open_overlay("approval-actions", None));

        assert_eq!(
            focus.rebuild(FocusTopology::new(view, None, false)),
            FocusIntent::Noop
        );
        assert_eq!(focus.overlay_depth(), 1);
        assert_eq!(
            focus.current().scope,
            FocusScope::Overlay("approval-actions".to_owned())
        );
        let restore = match focus.escape() {
            FocusIntent::CloseOverlay { restore, .. } => restore,
            other => panic!("expected close intent, got {other:?}"),
        };
        assert_eq!(restore.scope, FocusScope::Shell);
        assert_shell(&restore.anchor, Region::Timeline, Some("call-1"));
    }

    #[test]
    fn history_anchors_survive_diff_back_forward_and_topology_rebuilds() {
        let parent = session("parent");
        let snapshot = NavigationSnapshot::new(parent.clone());
        let mut navigation = NavigationState::new(parent.clone());
        assert!(navigation.open_diff(worktree("main"), &snapshot));
        let diff = navigation.current_view().clone();
        let mut focus = FocusReducer::new(FocusTopology::new(diff.clone(), None, false));
        expect_focus(focus.restore(
            FocusTopology::new(diff.clone(), None, false),
            navigation.focus().clone(),
        ));
        expect_focus(focus.reduce(FocusMove::Down));
        assert!(navigation.set_focus_anchor(focus.shell_anchor().clone()));

        assert!(navigation.back(&snapshot));
        let source_anchor = navigation.focus().clone();
        assert_shell(&source_anchor, Region::Timeline, Some(DIFF_ENTRY_ITEM_ID));
        let session_topology = FocusTopology::new(MainView::Session(parent), None, false);
        expect_focus(focus.restore(session_topology.clone(), source_anchor.clone()));
        assert_eq!(focus.rebuild(session_topology), FocusIntent::Noop);
        assert_eq!(focus.shell_anchor(), &source_anchor);

        assert!(navigation.forward(&snapshot));
        expect_focus(focus.restore(
            FocusTopology::new(diff, None, false),
            navigation.focus().clone(),
        ));
        assert_shell(focus.shell_anchor(), Region::DiffContent, None);
    }
}

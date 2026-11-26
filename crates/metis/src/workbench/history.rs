//! Local view navigation and independent workbench facts.

use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use super::identity::ToolCallKey;
use super::identity::{SessionKey, SubagentKey, WorktreeKey};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MainView {
    Session(SessionKey),
    #[allow(dead_code)] // Pure Diff routing is retained for the later native view.
    Diff {
        worktree: WorktreeKey,
        origin: SessionKey,
    },
    #[allow(dead_code)] // Pure Subagent routing is retained for the later native view.
    Subagent {
        key: SubagentKey,
    },
}

/// Semantic regions; stable item IDs address entries inside a region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Region {
    Navigation,
    Header,
    Timeline,
    Approval,
    Queue,
    Composer,
    SourceHeader,
    DiffContent,
    Transcript,
}

pub(super) const DIFF_ENTRY_ITEM_ID: &str = "worktree-diff";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FocusAnchor {
    pub(super) view: MainView,
    pub(super) region: Region,
    pub(super) item_id: Option<String>,
}

impl FocusAnchor {
    pub(super) fn new(view: MainView, region: Region) -> Self {
        Self {
            view,
            region,
            item_id: None,
        }
    }

    pub(super) fn with_item(mut self, item_id: impl Into<String>) -> Self {
        self.item_id = Some(item_id.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ScrollAnchor {
    pub(super) item_id: String,
    pub(super) offset: u64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TextSelection {
    pub(super) start: usize,
    pub(super) end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionLocalState {
    #[cfg(test)]
    pub(super) draft: String,
    #[cfg(test)]
    pub(super) selection: Option<TextSelection>,
    #[cfg(test)]
    pub(super) scroll: Option<ScrollAnchor>,
    pub(super) focus: FocusAnchor,
}

impl SessionLocalState {
    fn new(session: SessionKey) -> Self {
        Self {
            #[cfg(test)]
            draft: String::new(),
            #[cfg(test)]
            selection: None,
            #[cfg(test)]
            scroll: None,
            focus: FocusAnchor::new(MainView::Session(session), Region::Composer),
        }
    }
}

/// Injected ownership data, not a second Host facts implementation.
#[derive(Clone, Debug)]
pub(super) struct NavigationSnapshot {
    fallback: SessionKey,
    sessions: BTreeSet<SessionKey>,
    subagents: BTreeMap<SubagentKey, SubagentInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubagentInfo {
    continuable: bool,
}

impl NavigationSnapshot {
    pub(super) fn new(fallback: SessionKey) -> Self {
        Self {
            sessions: BTreeSet::from([fallback.clone()]),
            fallback,
            subagents: BTreeMap::new(),
        }
    }

    pub(super) fn add_session(&mut self, session: SessionKey) {
        self.sessions.insert(session);
    }

    pub(super) fn add_subagent(&mut self, key: SubagentKey, continuable: bool) {
        self.subagents.insert(key, SubagentInfo { continuable });
    }

    fn contains_view(&self, view: &MainView) -> bool {
        match view {
            MainView::Session(session) => self.sessions.contains(session),
            MainView::Diff { worktree, origin } => {
                self.sessions.contains(origin) && &origin.worktree == worktree
            }
            MainView::Subagent { key } => {
                self.sessions.contains(&key.parent) && self.subagents.contains_key(key)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HistoryEntry {
    view: MainView,
    focus: FocusAnchor,
}

/// Owns local navigation only. Host facts never appear in history entries.
#[derive(Clone, Debug)]
pub(super) struct NavigationState {
    current: HistoryEntry,
    back_entries: Vec<HistoryEntry>,
    forward_entries: Vec<HistoryEntry>,
    session_locals: BTreeMap<SessionKey, SessionLocalState>,
    subagent_scrolls: BTreeMap<SubagentKey, ScrollAnchor>,
}

impl NavigationState {
    pub(super) fn new(session: SessionKey) -> Self {
        let local = SessionLocalState::new(session.clone());
        let current = HistoryEntry {
            view: MainView::Session(session.clone()),
            focus: local.focus.clone(),
        };
        Self {
            current,
            back_entries: Vec::new(),
            forward_entries: Vec::new(),
            session_locals: BTreeMap::from([(session, local)]),
            subagent_scrolls: BTreeMap::new(),
        }
    }

    pub(super) fn prune(&mut self, snapshot: &NavigationSnapshot) {
        self.session_locals
            .retain(|key, _| snapshot.sessions.contains(key));
        self.subagent_scrolls
            .retain(|key, _| snapshot.sessions.contains(&key.parent));
    }

    pub(super) fn current_view(&self) -> &MainView {
        &self.current.view
    }

    pub(super) fn focus(&self) -> &FocusAnchor {
        &self.current.focus
    }

    #[cfg(test)]
    pub(super) fn set_focus(&mut self, region: Region, item_id: Option<String>) {
        self.current.focus = FocusAnchor {
            view: self.current.view.clone(),
            region,
            item_id,
        };
    }

    pub(super) fn set_focus_anchor(&mut self, anchor: FocusAnchor) -> bool {
        if anchor.view != self.current.view {
            return false;
        }
        self.current.focus = anchor;
        true
    }

    #[cfg(test)]
    pub(super) fn session_local(&self, session: &SessionKey) -> Option<&SessionLocalState> {
        self.session_locals.get(session)
    }

    pub(super) fn session_local_mut(&mut self, session: &SessionKey) -> &mut SessionLocalState {
        self.session_locals
            .entry(session.clone())
            .or_insert_with(|| SessionLocalState::new(session.clone()))
    }

    #[cfg(test)]
    pub(super) fn set_subagent_scroll(&mut self, key: SubagentKey, scroll: ScrollAnchor) {
        self.subagent_scrolls.insert(key, scroll);
    }

    #[cfg(test)]
    pub(super) fn subagent_scroll(&self, key: &SubagentKey) -> Option<&ScrollAnchor> {
        self.subagent_scrolls.get(key)
    }

    pub(super) fn open_session(
        &mut self,
        session: SessionKey,
        snapshot: &NavigationSnapshot,
    ) -> bool {
        self.navigate(MainView::Session(session), snapshot, None)
    }

    #[cfg(test)]
    pub(super) fn open_diff(
        &mut self,
        worktree: WorktreeKey,
        snapshot: &NavigationSnapshot,
    ) -> bool {
        let MainView::Session(origin) = &self.current.view else {
            return false;
        };
        let origin = origin.clone();
        let view = MainView::Diff { worktree, origin };
        if !snapshot.contains_view(&view) {
            return false;
        }

        self.set_source_focus(&view);
        self.navigate(view, snapshot, None)
    }

    #[cfg(test)]
    pub(super) fn open_subagent(
        &mut self,
        key: SubagentKey,
        snapshot: &NavigationSnapshot,
    ) -> bool {
        let MainView::Session(parent) = &self.current.view else {
            return false;
        };
        if parent != &key.parent {
            return false;
        }
        let view = MainView::Subagent { key };
        if !snapshot.contains_view(&view) {
            return false;
        }

        self.set_source_focus(&view);
        self.navigate(view, snapshot, None)
    }

    /// Restore a deep link. Invalid intent is discarded in favor of the known
    /// fallback Session instead of revealing a cross-owner or stale target.
    pub(super) fn restore(&mut self, view: MainView, snapshot: &NavigationSnapshot) -> bool {
        if !snapshot.contains_view(&view) {
            self.fallback_to_session(snapshot);
            return false;
        }
        self.set_source_focus(&view);
        self.navigate(view, snapshot, None)
    }

    pub(super) fn return_to_origin(&mut self, snapshot: &NavigationSnapshot) -> bool {
        let origin = match &self.current.view {
            MainView::Diff { origin, .. } => origin.clone(),
            MainView::Subagent { key } => key.parent.clone(),
            MainView::Session(_) => return false,
        };
        if !snapshot.sessions.contains(&origin) {
            self.fallback_to_session(snapshot);
            return false;
        }

        if self
            .back_entries
            .last()
            .is_some_and(|entry| entry.view == MainView::Session(origin.clone()))
        {
            return self.back(snapshot);
        }
        self.navigate(MainView::Session(origin), snapshot, None)
    }

    pub(super) fn back(&mut self, snapshot: &NavigationSnapshot) -> bool {
        let Some(target) = self.back_entries.pop() else {
            return false;
        };
        if !snapshot.contains_view(&target.view) {
            self.fallback_to_session(snapshot);
            return false;
        }

        self.save_current_session_focus();
        self.forward_entries.push(self.current.clone());
        self.current = target;
        self.sync_current_session_focus();
        true
    }

    #[cfg(test)]
    pub(super) fn forward(&mut self, snapshot: &NavigationSnapshot) -> bool {
        let Some(target) = self.forward_entries.pop() else {
            return false;
        };
        if !snapshot.contains_view(&target.view) {
            self.fallback_to_session(snapshot);
            return false;
        }

        self.save_current_session_focus();
        self.back_entries.push(self.current.clone());
        self.current = target;
        self.sync_current_session_focus();
        true
    }

    #[cfg(test)]
    pub(super) fn subagent_view(&self, snapshot: &NavigationSnapshot) -> Option<SubagentView> {
        let MainView::Subagent { key } = &self.current.view else {
            return None;
        };
        if !snapshot.contains_view(&self.current.view) {
            return None;
        }
        let info = snapshot.subagents.get(key)?;
        Some(SubagentView {
            key: key.clone(),
            mode: SubagentMode::ReadOnly,
            continuable: info.continuable,
        })
    }

    fn set_source_focus(&mut self, target: &MainView) {
        let focus = match (&self.current.view, target) {
            (MainView::Session(current), MainView::Diff { origin, .. }) if current == origin => {
                Some(
                    FocusAnchor::new(self.current.view.clone(), Region::Timeline)
                        .with_item(DIFF_ENTRY_ITEM_ID),
                )
            }
            (MainView::Session(current), MainView::Subagent { key }) if current == &key.parent => {
                Some(
                    FocusAnchor::new(self.current.view.clone(), Region::Timeline)
                        .with_item(key.subagent_id.clone()),
                )
            }
            _ => None,
        };
        if let Some(focus) = focus {
            self.current.focus = focus;
        }
    }

    fn navigate(
        &mut self,
        view: MainView,
        snapshot: &NavigationSnapshot,
        focus: Option<FocusAnchor>,
    ) -> bool {
        if !snapshot.contains_view(&view) {
            return false;
        }
        if self.current.view == view {
            return true;
        }

        self.save_current_session_focus();
        let focus = focus.unwrap_or_else(|| self.default_focus(&view));
        self.back_entries.push(self.current.clone());
        self.forward_entries.clear();
        self.current = HistoryEntry { view, focus };
        true
    }

    fn default_focus(&self, view: &MainView) -> FocusAnchor {
        match view {
            MainView::Session(session) => self.session_locals.get(session).map_or_else(
                || FocusAnchor::new(view.clone(), Region::Composer),
                |local| local.focus.clone(),
            ),
            MainView::Diff { .. } | MainView::Subagent { .. } => {
                FocusAnchor::new(view.clone(), Region::SourceHeader)
            }
        }
    }

    fn save_current_session_focus(&mut self) {
        if let MainView::Session(session) = &self.current.view {
            let session = session.clone();
            self.session_local_mut(&session).focus = self.current.focus.clone();
        }
    }

    fn sync_current_session_focus(&mut self) {
        if let MainView::Session(session) = &self.current.view {
            let session = session.clone();
            self.session_local_mut(&session).focus = self.current.focus.clone();
        }
    }

    fn fallback_to_session(&mut self, snapshot: &NavigationSnapshot) {
        self.save_current_session_focus();
        let view = MainView::Session(snapshot.fallback.clone());
        let focus = self.default_focus(&view);
        self.current = HistoryEntry { view, focus };
        self.back_entries.clear();
        self.forward_entries.clear();
        self.sync_current_session_focus();
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SubagentMode {
    ReadOnly,
}

/// Pure transcript projection; continuable never adds Session action regions.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SubagentView {
    pub(super) key: SubagentKey,
    pub(super) mode: SubagentMode,
    pub(super) continuable: bool,
}

#[cfg(test)]
#[allow(clippy::unused_self)]
impl SubagentView {
    pub(super) fn has_session_action_regions(&self) -> bool {
        false
    }
}

/// Observed decisions keyed by the original tool call. Recording a decision is
/// local projection, not a claim that the Host accepted a submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApprovalDecision {
    AllowOnce,
    Reject,
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ReviewFacts {
    pub(super) reviewed: bool,
    pub(super) feedback: Option<String>,
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct DomainFacts {
    reviews: BTreeMap<WorktreeKey, ReviewFacts>,
    approvals: BTreeMap<ToolCallKey, ApprovalDecision>,
}

#[cfg(test)]
impl DomainFacts {
    pub(super) fn mark_reviewed(&mut self, worktree: WorktreeKey) {
        self.reviews.entry(worktree).or_default().reviewed = true;
    }

    /// Returns false after the single feedback slot has been filled.
    pub(super) fn send_feedback(
        &mut self,
        worktree: WorktreeKey,
        feedback: impl Into<String>,
    ) -> bool {
        let review = self.reviews.entry(worktree).or_default();
        if review.feedback.is_some() {
            return false;
        }
        review.feedback = Some(feedback.into());
        true
    }

    pub(super) fn record_approval(&mut self, call: ToolCallKey, decision: ApprovalDecision) {
        self.approvals.insert(call, decision);
    }

    pub(super) fn review(&self, worktree: &WorktreeKey) -> Option<&ReviewFacts> {
        self.reviews.get(worktree)
    }

    pub(super) fn approval(&self, call: &ToolCallKey) -> Option<ApprovalDecision> {
        self.approvals.get(call).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbench::identity::{ProjectKey, WorktreeKey};

    fn worktree(id: &str) -> WorktreeKey {
        WorktreeKey::new(ProjectKey::new("host", "project"), id)
    }

    fn session(worktree_id: &str, id: &str) -> SessionKey {
        SessionKey::new(worktree(worktree_id), id)
    }

    fn scroll(item_id: &str, offset: u64) -> ScrollAnchor {
        ScrollAnchor {
            item_id: item_id.to_owned(),
            offset,
        }
    }

    #[test]
    fn diff_belongs_to_worktree_and_origin_restores_session_state() {
        let parent = session("main", "parent");
        let snapshot = NavigationSnapshot::new(parent.clone());
        let mut navigation = NavigationState::new(parent.clone());
        {
            let local = navigation.session_local_mut(&parent);
            local.draft = "draft".to_owned();
            local.selection = Some(TextSelection { start: 1, end: 3 });
            local.scroll = Some(scroll("message-7", 12));
        }
        navigation.set_focus(Region::Composer, None);

        assert!(navigation.open_diff(worktree("main"), &snapshot));
        assert_eq!(
            navigation.current_view(),
            &MainView::Diff {
                worktree: worktree("main"),
                origin: parent.clone(),
            }
        );

        assert!(navigation.return_to_origin(&snapshot));
        assert_eq!(
            navigation.current_view(),
            &MainView::Session(parent.clone())
        );
        assert_eq!(navigation.focus().region, Region::Timeline);
        assert_eq!(
            navigation.focus().item_id.as_deref(),
            Some(DIFF_ENTRY_ITEM_ID)
        );
        assert_eq!(navigation.focus().view, MainView::Session(parent.clone()));
        let local = navigation
            .session_local(&parent)
            .expect("parent local state");
        assert_eq!(local.draft, "draft");
        assert_eq!(local.selection, Some(TextSelection { start: 1, end: 3 }));
        assert_eq!(local.scroll, Some(scroll("message-7", 12)));

        assert!(navigation.forward(&snapshot));
        assert_eq!(
            navigation.current_view(),
            &MainView::Diff {
                worktree: worktree("main"),
                origin: parent,
            }
        );
    }

    #[test]
    fn diff_rejects_cross_worktree_and_unknown_origin_targets() {
        let parent = session("main", "parent");
        let snapshot = NavigationSnapshot::new(parent.clone());
        let mut navigation = NavigationState::new(parent.clone());

        assert!(!navigation.open_diff(worktree("other"), &snapshot));
        assert_eq!(
            navigation.current_view(),
            &MainView::Session(parent.clone())
        );

        let unknown_origin = session("main", "unknown");
        assert!(!navigation.restore(
            MainView::Diff {
                worktree: worktree("main"),
                origin: unknown_origin,
            },
            &snapshot,
        ));
        assert_eq!(navigation.current_view(), &MainView::Session(parent));
        assert!(!navigation.back(&snapshot));
        assert!(!navigation.forward(&snapshot));
    }

    #[test]
    fn restored_diff_still_records_its_origin_entry_focus() {
        let parent = session("main", "parent");
        let snapshot = NavigationSnapshot::new(parent.clone());
        let mut navigation = NavigationState::new(parent.clone());
        navigation.set_focus(Region::Composer, None);

        assert!(navigation.restore(
            MainView::Diff {
                worktree: worktree("main"),
                origin: parent.clone(),
            },
            &snapshot,
        ));
        assert!(navigation.return_to_origin(&snapshot));
        assert_eq!(
            navigation.current_view(),
            &MainView::Session(parent.clone())
        );
        assert_eq!(navigation.focus().region, Region::Timeline);
        assert_eq!(
            navigation.focus().item_id.as_deref(),
            Some(DIFF_ENTRY_ITEM_ID)
        );
        assert_eq!(navigation.focus().view, MainView::Session(parent));
    }

    #[test]
    fn reviewed_single_feedback_and_approval_do_not_fill_each_other() {
        let mut facts = DomainFacts::default();
        let reviewed = worktree("reviewed");
        let feedback_only = worktree("feedback");
        let approved = worktree("approved");
        let call = ToolCallKey::new(session("approved", "session"), "call");

        facts.mark_reviewed(reviewed.clone());
        assert_eq!(
            facts.review(&reviewed),
            Some(&ReviewFacts {
                reviewed: true,
                feedback: None,
            })
        );
        assert_eq!(facts.approval(&call), None);

        assert!(facts.send_feedback(feedback_only.clone(), "first"));
        assert!(!facts.send_feedback(feedback_only.clone(), "second"));
        assert_eq!(
            facts.review(&feedback_only),
            Some(&ReviewFacts {
                reviewed: false,
                feedback: Some("first".to_owned()),
            })
        );
        assert_eq!(facts.approval(&call), None);

        facts.record_approval(call.clone(), ApprovalDecision::AllowOnce);
        assert_eq!(facts.approval(&call), Some(ApprovalDecision::AllowOnce));
        assert_eq!(facts.review(&approved), None);
    }

    #[test]
    fn history_preserves_review_feedback_and_approval_facts() {
        let parent = session("main", "parent");
        let snapshot = NavigationSnapshot::new(parent.clone());
        let mut navigation = NavigationState::new(parent);
        let mut facts = DomainFacts::default();
        let call = ToolCallKey::new(session("main", "parent"), "call");
        facts.mark_reviewed(worktree("main"));
        assert!(facts.send_feedback(worktree("main"), "feedback"));
        facts.record_approval(call, ApprovalDecision::Reject);
        let before = facts.clone();

        assert!(navigation.open_diff(worktree("main"), &snapshot));
        assert!(navigation.back(&snapshot));
        assert!(navigation.forward(&snapshot));
        assert!(navigation.back(&snapshot));

        assert_eq!(facts, before);
    }

    #[test]
    fn subagent_validates_parent_and_continuable_stays_read_only() {
        let parent = session("main", "parent");
        let other = session("main", "other");
        let child = SubagentKey::new(parent.clone(), "child");
        let foreign = SubagentKey::new(other.clone(), "foreign");
        let mut snapshot = NavigationSnapshot::new(parent.clone());
        snapshot.add_session(other);
        snapshot.add_subagent(child.clone(), true);
        snapshot.add_subagent(foreign.clone(), false);
        let mut navigation = NavigationState::new(parent);

        assert!(!navigation.open_subagent(foreign, &snapshot));
        assert!(navigation.open_subagent(child.clone(), &snapshot));
        let view = navigation
            .subagent_view(&snapshot)
            .expect("valid child view");
        assert_eq!(view.key, child);
        assert_eq!(view.mode, SubagentMode::ReadOnly);
        assert!(view.continuable);
        assert!(!view.has_session_action_regions());
    }

    #[test]
    fn subagent_return_restores_child_row_and_each_child_keeps_scroll() {
        let parent = session("main", "parent");
        let child_a = SubagentKey::new(parent.clone(), "child-a");
        let child_b = SubagentKey::new(parent.clone(), "child-b");
        let mut snapshot = NavigationSnapshot::new(parent.clone());
        snapshot.add_subagent(child_a.clone(), false);
        snapshot.add_subagent(child_b.clone(), true);
        let mut navigation = NavigationState::new(parent.clone());
        navigation.session_local_mut(&parent).draft = "parent draft".to_owned();
        navigation.set_subagent_scroll(child_a.clone(), scroll("a-message", 10));
        navigation.set_subagent_scroll(child_b.clone(), scroll("b-message", 20));

        assert!(navigation.open_subagent(child_a.clone(), &snapshot));
        assert!(navigation.back(&snapshot));
        assert_eq!(navigation.focus().region, Region::Timeline);
        assert_eq!(navigation.focus().item_id.as_deref(), Some("child-a"));
        assert_eq!(
            navigation.session_local(&parent).expect("local").draft,
            "parent draft"
        );

        assert!(navigation.open_subagent(child_b.clone(), &snapshot));
        assert_eq!(
            navigation.subagent_scroll(&child_a),
            Some(&scroll("a-message", 10))
        );
        assert_eq!(
            navigation.subagent_scroll(&child_b),
            Some(&scroll("b-message", 20))
        );
        assert!(navigation.back(&snapshot));
        assert_eq!(navigation.focus().item_id.as_deref(), Some("child-b"));
        assert_eq!(
            navigation.subagent_scroll(&child_a),
            Some(&scroll("a-message", 10))
        );
    }

    #[test]
    fn back_revalidates_stale_targets_without_rolling_back_facts() {
        let parent = session("main", "parent");
        let child = SubagentKey::new(parent.clone(), "child");
        let other = session("main", "other");
        let mut snapshot = NavigationSnapshot::new(parent.clone());
        snapshot.add_session(other.clone());
        snapshot.add_subagent(child.clone(), false);
        let mut navigation = NavigationState::new(parent.clone());
        let mut facts = DomainFacts::default();
        facts.mark_reviewed(worktree("main"));
        let before = facts.clone();

        assert!(navigation.open_subagent(child, &snapshot));
        assert!(navigation.open_session(other, &snapshot));
        let stale_snapshot = NavigationSnapshot::new(parent.clone());
        assert!(!navigation.back(&stale_snapshot));
        assert_eq!(navigation.current_view(), &MainView::Session(parent));
        assert_eq!(navigation.focus().region, Region::Timeline);
        assert!(!navigation.forward(&stale_snapshot));
        assert_eq!(facts, before);
    }

    #[test]
    fn session_navigation_restores_local_focus_without_changing_selection() {
        let first = session("main", "first");
        let second = session("main", "second");
        let mut snapshot = NavigationSnapshot::new(first.clone());
        snapshot.add_session(second.clone());
        let mut navigation = NavigationState::new(first.clone());
        navigation.set_focus(Region::Timeline, Some("call-1".to_owned()));
        navigation.session_local_mut(&first).selection = Some(TextSelection { start: 2, end: 4 });

        assert!(navigation.open_session(second.clone(), &snapshot));
        assert_eq!(navigation.focus().region, Region::Composer);
        assert!(navigation.back(&snapshot));
        assert_eq!(navigation.focus().region, Region::Timeline);
        assert_eq!(navigation.focus().item_id.as_deref(), Some("call-1"));
        assert_eq!(
            navigation
                .session_local(&first)
                .expect("first local")
                .selection,
            Some(TextSelection { start: 2, end: 4 })
        );
    }
}

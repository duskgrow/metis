use gpui_kit::{
    App, Context, Entity, FocusHandle, Focusable as _, Render, Window,
    base::{Selectable as _, TestSupportExt as _},
    component::{
        button::{Button, ButtonGroup},
        diff::{Diff, DiffFile, DiffMode, DiffState},
    },
    div,
    prelude::*,
};

use super::{EffectResult, host_bridge::GitDiffResult};

enum DiffStatus {
    Loading,
    Offline,
    Empty { stale: bool },
    Ready { stale: bool },
    Error(String),
}

pub(super) struct DiffReview {
    state: Entity<DiffState>,
    request_id: Option<u64>,
    status: DiffStatus,
}

impl DiffReview {
    pub(super) fn new(request: Option<(u64, EffectResult)>, cx: &mut Context<Self>) -> Self {
        let (request_id, status) = match request {
            Some((id, EffectResult::Recorded)) => (Some(id), DiffStatus::Loading),
            Some((_, EffectResult::Unavailable)) => (
                None,
                DiffStatus::Error(
                    "Host connection is unavailable; the patch was not fetched.".to_owned(),
                ),
            ),
            None => (None, DiffStatus::Offline),
        };
        Self {
            state: cx.new(|cx| DiffState::new(Vec::<DiffFile>::new(), cx)),
            request_id,
            status,
        }
    }

    pub(super) fn complete(&mut self, result: GitDiffResult, cx: &mut Context<Self>) {
        if self.request_id != Some(result.request_id) {
            return;
        }
        self.request_id = None;
        match result.result {
            Ok(patch) => match DiffFile::parse(&patch) {
                Ok(files) => {
                    let empty = files.is_empty();
                    self.state
                        .update(cx, |state, cx| state.set_files(files, cx));
                    self.status = if empty {
                        DiffStatus::Empty { stale: false }
                    } else {
                        DiffStatus::Ready { stale: false }
                    };
                }
                Err(error) => {
                    self.status =
                        DiffStatus::Error(format!("Git patch could not be displayed: {error}"));
                }
            },
            Err(error) => {
                self.status = DiffStatus::Error(format!("Host git/diff request failed: {error}"));
            }
        }
        cx.notify();
    }

    pub(super) fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.status = match std::mem::replace(&mut self.status, DiffStatus::Offline) {
            DiffStatus::Loading => DiffStatus::Error(
                "Host disconnected before the Worktree diff was received.".to_owned(),
            ),
            DiffStatus::Ready { .. } => DiffStatus::Ready { stale: true },
            DiffStatus::Empty { .. } => DiffStatus::Empty { stale: true },
            other => other,
        };
        self.request_id = None;
        cx.notify();
    }

    pub(super) fn content_focus(&self, cx: &App) -> Option<FocusHandle> {
        matches!(&self.status, DiffStatus::Ready { .. })
            .then(|| self.state.read(cx).focus_handle(cx))
    }

    #[cfg(test)]
    pub(super) fn state(&self) -> Entity<DiffState> {
        self.state.clone()
    }
}

impl Render for DiffReview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.state.read(cx).mode();
        let mut content = div()
            .id("worktree-diff-review")
            .test_support()
            .flex()
            .flex_col()
            .size_full();
        if matches!(
            &self.status,
            DiffStatus::Ready { stale: true } | DiffStatus::Empty { stale: true }
        ) {
            content = content.child(
                div()
                    .id("worktree-diff-stale")
                    .test_support()
                    .child("Host disconnected. This patch may no longer match the Worktree."),
            );
        }
        match &self.status {
            DiffStatus::Loading => content.child(
                div()
                    .id("worktree-diff-loading")
                    .test_support()
                    .child("Loading the current Worktree diff…"),
            ),
            DiffStatus::Offline => content.child(
                div()
                    .id("worktree-diff-offline")
                    .test_support()
                    .child("Host is offline. The current Worktree diff was not fetched."),
            ),
            DiffStatus::Empty { .. } => content.child(
                div()
                    .id("worktree-diff-empty")
                    .test_support()
                    .child("No tracked file changes in this Worktree."),
            ),
            DiffStatus::Ready { .. } => content
                .child(
                    ButtonGroup::new("diff-mode")
                        .compact()
                        .children(vec![
                            Button::new("diff-unified")
                                .label("Unified")
                                .selected(mode == DiffMode::Unified),
                            Button::new("diff-split")
                                .label("Split")
                                .selected(mode == DiffMode::Split),
                        ])
                        .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                            let mode = if selected.contains(&1) {
                                DiffMode::Split
                            } else {
                                DiffMode::Unified
                            };
                            this.state.update(cx, |state, cx| state.set_mode(mode, cx));
                        })),
                )
                .child(
                    div()
                        .id("worktree-diff-content")
                        .test_support()
                        .flex_1()
                        .min_h_0()
                        .child(Diff::new(&self.state).size_full()),
                ),
            DiffStatus::Error(message) => content.child(
                div()
                    .id("worktree-diff-error")
                    .test_support()
                    .child(message.clone()),
            ),
        }
    }
}

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{
    App, Context, Entity, Render, Subscription, WeakEntity, Window,
    component::{
        list::ListItem,
        tree::{Tree, TreeEvent, TreeItem, TreeState},
    },
    div,
    prelude::*,
};

use super::{SessionKey, Workbench, WorkbenchProjection, logical_id, session_id};
use crate::workbench::identity::{ProjectKey, WorktreeKey};

#[derive(Clone)]
enum ExpansionTarget {
    Project(ProjectKey),
    Worktree(WorktreeKey),
    Subagent(SessionKey),
}

pub(super) struct SessionNavigation {
    tree: Entity<TreeState>,
    targets: BTreeMap<String, SessionKey>,
    expansion_targets: BTreeMap<String, ExpansionTarget>,
    expanded_projects: BTreeSet<ProjectKey>,
    expanded_worktrees: BTreeSet<WorktreeKey>,
    expanded_subagents: BTreeSet<SessionKey>,
    parent: WeakEntity<Workbench>,
    _tree_subscription: Subscription,
}

impl SessionNavigation {
    pub(super) fn new(
        projection: &WorkbenchProjection,
        selected: Option<&SessionKey>,
        parent: WeakEntity<Workbench>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut expanded_projects = BTreeSet::new();
        let mut expanded_worktrees = BTreeSet::new();
        let expanded_subagents = BTreeSet::new();
        if let Some(selected) = selected {
            expanded_projects.insert(selected.worktree.project.clone());
            expanded_worktrees.insert(selected.worktree.clone());
        }
        let (items, targets, expansion_targets) = build_items(
            projection,
            &expanded_projects,
            &expanded_worktrees,
            &expanded_subagents,
        );
        let tree = cx.new(|cx| TreeState::new(cx).items(items));
        if let Some(selected) = selected {
            let id = session_id(selected);
            tree.update(cx, |tree, cx| {
                tree.set_selected_item(Some(&TreeItem::new(id, "")), cx);
            });
        }
        let tree_subscription = cx.subscribe(&tree, |this, _, event, _| {
            let (id, expanded) = match event {
                TreeEvent::Expanded(id) => (id, true),
                TreeEvent::Collapsed(id) => (id, false),
            };
            let Some(target) = this.expansion_targets.get(id.as_str()) else {
                return;
            };
            match target {
                ExpansionTarget::Project(key) => {
                    if expanded {
                        this.expanded_projects.insert(key.clone());
                    } else {
                        this.expanded_projects.remove(key);
                    }
                }
                ExpansionTarget::Worktree(key) => {
                    if expanded {
                        this.expanded_worktrees.insert(key.clone());
                    } else {
                        this.expanded_worktrees.remove(key);
                    }
                }
                ExpansionTarget::Subagent(key) => {
                    if expanded {
                        this.expanded_subagents.insert(key.clone());
                    } else {
                        this.expanded_subagents.remove(key);
                    }
                }
            }
        });
        Self {
            tree,
            targets,
            expansion_targets,
            expanded_projects,
            expanded_worktrees,
            expanded_subagents,
            parent,
            _tree_subscription: tree_subscription,
        }
    }

    pub(super) fn tree_state(&self, _: &App) -> Entity<TreeState> {
        self.tree.clone()
    }

    #[cfg(test)]
    pub(super) fn replace_projection(
        &mut self,
        projection: &WorkbenchProjection,
        selected: Option<&SessionKey>,
        cx: &mut Context<Self>,
    ) {
        let live_projects = projection
            .sessions
            .iter()
            .map(|session| session.key.worktree.project.clone())
            .collect::<BTreeSet<_>>();
        let live_worktrees = projection
            .sessions
            .iter()
            .map(|session| session.key.worktree.clone())
            .collect::<BTreeSet<_>>();
        self.expanded_projects
            .retain(|key| live_projects.contains(key));
        self.expanded_worktrees
            .retain(|key| live_worktrees.contains(key));
        self.expanded_subagents.retain(|key| {
            projection
                .sessions
                .iter()
                .any(|session| &session.key == key)
        });
        let (items, targets, expansion_targets) = build_items(
            projection,
            &self.expanded_projects,
            &self.expanded_worktrees,
            &self.expanded_subagents,
        );
        self.targets = targets;
        self.expansion_targets = expansion_targets;
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            if let Some(selected) = selected {
                tree.set_selected_item(Some(&TreeItem::new(session_id(selected), "")), cx);
            } else {
                tree.set_selected_index(None, cx);
            }
        });
    }

    fn tokens(&self, cx: &App) -> super::ResolvedTokens {
        // Navigation reads the complete Rust snapshot through its owner; no
        // value is recovered from Component or Base's lossy theme projection.
        self.parent
            .upgrade()
            .expect("navigation owner is alive")
            .read(cx)
            .theme
            .resolved()
            .clone()
    }
}

impl Render for SessionNavigation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree.clone();
        let targets = self.targets.clone();
        let tokens = self.tokens(cx);
        let empty = targets.is_empty();
        let owner = self.parent.upgrade().expect("navigation owner is alive");
        let tree_element = Tree::new(&tree, move |_, entry, _, window, _| {
            let id = entry.item().id.clone();
            let label = entry.item().label.clone();
            let item = ListItem::new(id.clone()).child(label);
            let Some(session) = targets.get(id.as_str()).cloned() else {
                return item;
            };
            let owner = owner.clone();
            let listener =
                window.listener_for(&owner, move |this, _: &gpui_kit::ClickEvent, window, cx| {
                    this.open_session(session.clone(), window, cx);
                });
            item.on_click(move |event, window, cx| listener(event, window, cx))
        });
        div()
            .flex_col()
            .size_full()
            .p(tokens.metis.density_gap)
            .child(
                div()
                    .text_color(tokens.metis.foreground_secondary)
                    .text_size(tokens.metis.metadata_font_size)
                    .child(if empty {
                        "Offline · no Host or Sessions"
                    } else {
                        "Host / Project / Worktree / Session"
                    }),
            )
            .child(div().flex_1().min_h_0().child(tree_element))
    }
}

fn build_items(
    projection: &WorkbenchProjection,
    expanded_projects: &BTreeSet<ProjectKey>,
    expanded_worktrees: &BTreeSet<WorktreeKey>,
    expanded_subagents: &BTreeSet<SessionKey>,
) -> (
    Vec<TreeItem>,
    BTreeMap<String, SessionKey>,
    BTreeMap<String, ExpansionTarget>,
) {
    let mut groups: BTreeMap<
        String,
        BTreeMap<ProjectKey, BTreeMap<WorktreeKey, Vec<&super::SessionProjection>>>,
    > = BTreeMap::new();
    for session in &projection.sessions {
        groups
            .entry(session.key.worktree.project.host_id.clone())
            .or_default()
            .entry(session.key.worktree.project.clone())
            .or_default()
            .entry(session.key.worktree.clone())
            .or_default()
            .push(session);
    }

    let mut roots = Vec::new();
    let mut targets = BTreeMap::new();
    let mut expansion_targets = BTreeMap::new();
    for (host, projects) in groups {
        let mut project_items = Vec::new();
        for (project, worktrees) in projects {
            let project_id = logical_id("project", &[&project.host_id, &project.project_id]);
            expansion_targets.insert(
                project_id.clone(),
                ExpansionTarget::Project(project.clone()),
            );
            let mut worktree_items = Vec::new();
            for (worktree, sessions) in worktrees {
                let worktree_id = logical_id(
                    "worktree",
                    &[
                        &worktree.project.host_id,
                        &worktree.project.project_id,
                        &worktree.worktree_id,
                    ],
                );
                expansion_targets.insert(
                    worktree_id.clone(),
                    ExpansionTarget::Worktree(worktree.clone()),
                );
                let mut session_items = Vec::new();
                for session in sessions {
                    let id = session_id(&session.key);
                    targets.insert(id.clone(), session.key.clone());
                    let mut item = TreeItem::new(id, session.title.clone());
                    if !session.subagents.is_empty() {
                        let subagent_items = session
                            .subagents
                            .iter()
                            .map(|subagent| {
                                let subagent_id = logical_id(
                                    "subagent",
                                    &[&session_id(&session.key), &subagent.key.subagent_id],
                                );
                                expansion_targets.insert(
                                    subagent_id.clone(),
                                    ExpansionTarget::Subagent(session.key.clone()),
                                );
                                TreeItem::new(
                                    subagent_id,
                                    format!("{} · read-only model only", subagent.title),
                                )
                                .disabled(true)
                            })
                            .collect::<Vec<_>>();
                        let session_expanded = expanded_subagents.contains(&session.key);
                        expansion_targets.insert(
                            session_id(&session.key),
                            ExpansionTarget::Subagent(session.key.clone()),
                        );
                        item = item.expanded(session_expanded).children(subagent_items);
                    }
                    session_items.push(item);
                }
                worktree_items.push(
                    TreeItem::new(worktree_id, worktree.worktree_id.clone())
                        .expanded(expanded_worktrees.contains(&worktree))
                        .children(session_items),
                );
            }
            project_items.push(
                TreeItem::new(project_id, project.project_id.clone())
                    .expanded(expanded_projects.contains(&project))
                    .children(worktree_items),
            );
        }
        roots.push(
            TreeItem::new(logical_id("host", &[&host]), host)
                .expanded(true)
                .children(project_items),
        );
    }
    (roots, targets, expansion_targets)
}

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
            reveal_session(selected, &mut expanded_projects, &mut expanded_worktrees);
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

    pub(super) fn select_session(
        &mut self,
        projection: &WorkbenchProjection,
        selected: &SessionKey,
        cx: &mut Context<Self>,
    ) {
        reveal_session(
            selected,
            &mut self.expanded_projects,
            &mut self.expanded_worktrees,
        );
        self.rebuild(projection, Some(selected), true, cx);
    }

    #[cfg(test)]
    pub(super) fn replace_projection(
        &mut self,
        projection: &WorkbenchProjection,
        selected: Option<&SessionKey>,
        reveal_selected: bool,
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
        let live_sessions = projection
            .sessions
            .iter()
            .map(|session| &session.key)
            .collect::<BTreeSet<_>>();
        self.expanded_projects
            .retain(|key| live_projects.contains(key));
        self.expanded_worktrees
            .retain(|key| live_worktrees.contains(key));
        self.expanded_subagents
            .retain(|key| live_sessions.contains(key));
        if reveal_selected && let Some(selected) = selected {
            reveal_session(
                selected,
                &mut self.expanded_projects,
                &mut self.expanded_worktrees,
            );
        }
        self.rebuild(projection, selected, reveal_selected, cx);
    }

    fn rebuild(
        &mut self,
        projection: &WorkbenchProjection,
        selected: Option<&SessionKey>,
        reveal_selected: bool,
        cx: &mut Context<Self>,
    ) {
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
                let item = TreeItem::new(session_id(selected), "");
                if reveal_selected || tree.index_of(&item.id).is_some() {
                    tree.set_selected_item(Some(&item), cx);
                } else {
                    tree.set_selected_index(None, cx);
                }
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

fn reveal_session(
    selected: &SessionKey,
    expanded_projects: &mut BTreeSet<ProjectKey>,
    expanded_worktrees: &mut BTreeSet<WorktreeKey>,
) {
    expanded_projects.insert(selected.worktree.project.clone());
    expanded_worktrees.insert(selected.worktree.clone());
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
            let has_multiple_worktrees = worktrees.len() > 1;
            let mut project_children = Vec::new();
            for (worktree, sessions) in worktrees {
                let worktree_id = logical_id(
                    "worktree",
                    &[
                        &worktree.project.host_id,
                        &worktree.project.project_id,
                        &worktree.worktree_id,
                    ],
                );
                if has_multiple_worktrees {
                    expansion_targets.insert(
                        worktree_id.clone(),
                        ExpansionTarget::Worktree(worktree.clone()),
                    );
                }
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
                if has_multiple_worktrees {
                    project_children.push(
                        TreeItem::new(worktree_id, worktree.worktree_id.clone())
                            .expanded(expanded_worktrees.contains(&worktree))
                            .children(session_items),
                    );
                } else {
                    project_children.extend(session_items);
                }
            }
            project_items.push(
                TreeItem::new(project_id, project.project_id.clone())
                    .expanded(expanded_projects.contains(&project))
                    .children(project_children),
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

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::workbench::{
        identity::SubagentKey,
        session::{
            ConnectionState, ExecutionState, SessionPermissionPolicy, SessionProjection,
            SubagentProjection,
        },
        stats::StatsInput,
    };

    fn session(
        host: &str,
        project: &str,
        worktree: &str,
        id: &str,
        title: &str,
    ) -> SessionProjection {
        SessionProjection {
            key: SessionKey::new(
                WorktreeKey::new(ProjectKey::new(host, project), worktree),
                id,
            ),
            title: title.to_owned(),
            generation: 1,
            execution: ExecutionState::NotExecuted,
            items: Rc::new(Vec::new()),
            queue: Vec::new(),
            permission: SessionPermissionPolicy::AskEachTime,
            models: Vec::new(),
            initial_model: None,
            stats: StatsInput::default(),
            subagents: Vec::new(),
        }
    }

    fn prototype() -> WorkbenchProjection {
        let mut deployment = session("h-local", "p-local-atlas", "w-atlas", "s5", "更新部署手册");
        deployment.subagents = vec![
            SubagentProjection {
                key: SubagentKey::new(deployment.key.clone(), "child-running"),
                title: "检查部署环境".to_owned(),
                continuable: true,
            },
            SubagentProjection {
                key: SubagentKey::new(deployment.key.clone(), "child-complete"),
                title: "核对文档链接".to_owned(),
                continuable: false,
            },
        ];
        WorkbenchProjection {
            connection: ConnectionState::Online,
            sessions: vec![
                session(
                    "h-local",
                    "p-local-metis",
                    "w-main",
                    "s1",
                    "修复重连后的旧审批",
                ),
                session(
                    "h-local",
                    "p-local-metis",
                    "w-main",
                    "s2",
                    "补充重连回归测试",
                ),
                session(
                    "h-build",
                    "p-build-metis",
                    "w-release",
                    "s4",
                    "打包 portable host",
                ),
                deployment,
                session("h-local", "p-local-atlas", "w-atlas", "s6", "检查文档链接"),
                session(
                    "h-local",
                    "p-local-atlas",
                    "w-review",
                    "s3",
                    "审查差异提示文案",
                ),
            ],
        }
    }

    fn project_key(host: &str, project: &str) -> ProjectKey {
        ProjectKey::new(host, project)
    }

    fn worktree_key(host: &str, project: &str, worktree: &str) -> WorktreeKey {
        WorktreeKey::new(project_key(host, project), worktree)
    }

    fn worktree_id(host: &str, project: &str, worktree: &str) -> String {
        logical_id("worktree", &[host, project, worktree])
    }

    fn ids(items: &[TreeItem]) -> Vec<String> {
        items.iter().map(|item| item.id.to_string()).collect()
    }

    fn project<'a>(roots: &'a [TreeItem], host: &str, project: &str) -> &'a TreeItem {
        let host_id = logical_id("host", &[host]);
        let project_id = logical_id("project", &[host, project]);
        roots
            .iter()
            .find(|item| item.id.as_str() == host_id)
            .and_then(|host| {
                host.children
                    .iter()
                    .find(|item| item.id.as_str() == project_id)
            })
            .unwrap_or_else(|| panic!("missing {host}/{project}"))
    }

    fn child<'a>(item: &'a TreeItem, id: &str) -> &'a TreeItem {
        item.children
            .iter()
            .find(|item| item.id.as_str() == id)
            .unwrap_or_else(|| panic!("missing child {id}"))
    }

    #[test]
    fn prototype_flattens_single_worktrees_and_reveals_only_the_selected_path() {
        let projection = prototype();
        let selected = projection
            .sessions
            .iter()
            .find(|session| session.key.session_id == "s3")
            .expect("s3")
            .key
            .clone();
        let mut projects = BTreeSet::new();
        let mut worktrees = BTreeSet::new();
        reveal_session(&selected, &mut projects, &mut worktrees);

        assert_eq!(
            projects,
            BTreeSet::from([project_key("h-local", "p-local-atlas")])
        );
        assert_eq!(
            worktrees,
            BTreeSet::from([worktree_key("h-local", "p-local-atlas", "w-review")])
        );

        let (roots, targets, expansion_targets) =
            build_items(&projection, &projects, &worktrees, &BTreeSet::new());
        assert_eq!(roots.len(), 2, "the fixture has two distinct Hosts");
        assert_eq!(targets.len(), 6, "subagents are not domain Sessions");

        let metis = project(&roots, "h-local", "p-local-metis");
        assert!(!metis.is_expanded());
        assert_eq!(
            ids(&metis.children),
            vec![
                session_id(&projection.sessions[0].key),
                session_id(&projection.sessions[1].key)
            ],
            "a single Worktree is flattened into its Project"
        );
        assert!(!expansion_targets.contains_key(&worktree_id(
            "h-local",
            "p-local-metis",
            "w-main"
        )));
        let build = project(&roots, "h-build", "p-build-metis");
        assert_eq!(
            ids(&build.children),
            vec![session_id(&projection.sessions[2].key)]
        );

        let atlas = project(&roots, "h-local", "p-local-atlas");
        assert!(atlas.is_expanded());
        assert_eq!(
            ids(&atlas.children),
            vec![
                worktree_id("h-local", "p-local-atlas", "w-atlas"),
                worktree_id("h-local", "p-local-atlas", "w-review"),
            ]
        );
        assert!(!child(atlas, &worktree_id("h-local", "p-local-atlas", "w-atlas")).is_expanded());
        assert!(child(atlas, &worktree_id("h-local", "p-local-atlas", "w-review")).is_expanded());
    }

    #[test]
    fn revealing_sessions_preserves_independent_project_worktree_and_subagent_expansion() {
        let projection = prototype();
        let selected = |id: &str| {
            projection
                .sessions
                .iter()
                .find(|session| session.key.session_id == id)
                .unwrap_or_else(|| panic!("missing {id}"))
                .key
                .clone()
        };
        let mut projects = BTreeSet::new();
        let mut worktrees = BTreeSet::new();
        for id in ["s3", "s1", "s5"] {
            reveal_session(&selected(id), &mut projects, &mut worktrees);
        }
        let subagents = BTreeSet::from([selected("s5")]);

        assert_eq!(
            projects,
            BTreeSet::from([
                project_key("h-local", "p-local-atlas"),
                project_key("h-local", "p-local-metis"),
            ])
        );
        assert_eq!(
            worktrees,
            BTreeSet::from([
                worktree_key("h-local", "p-local-atlas", "w-atlas"),
                worktree_key("h-local", "p-local-atlas", "w-review"),
                worktree_key("h-local", "p-local-metis", "w-main"),
            ]),
            "revealing a Session must not collapse another Worktree"
        );

        projects.remove(&project_key("h-local", "p-local-atlas"));
        let (roots, _, _) = build_items(&projection, &projects, &worktrees, &subagents);
        let atlas = project(&roots, "h-local", "p-local-atlas");
        assert!(!atlas.is_expanded());
        let main = child(atlas, &worktree_id("h-local", "p-local-atlas", "w-atlas"));
        let review = child(atlas, &worktree_id("h-local", "p-local-atlas", "w-review"));
        assert!(main.is_expanded(), "Project collapse hides, not resets");
        assert!(review.is_expanded(), "Project collapse hides, not resets");
        assert!(
            child(main, &session_id(&selected("s5"))).is_expanded(),
            "Subagent expansion is independent of both ancestor sets"
        );
    }
}

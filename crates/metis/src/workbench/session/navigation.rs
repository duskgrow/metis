use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{
    App, Context, Entity, FocusHandle, Render, Subscription, WeakEntity, Window,
    base::Disableable as _,
    component::{
        button::{Button, ButtonVariants as _},
        list::ListItem,
        menu::DropdownMenu as _,
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

#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(namespace = metis_workbench, no_json)]
struct SelectHost(Option<String>);

pub(super) struct SessionNavigation {
    tree: Entity<TreeState>,
    targets: BTreeMap<String, SessionKey>,
    expansion_targets: BTreeMap<String, ExpansionTarget>,
    expanded_projects: BTreeSet<ProjectKey>,
    expanded_worktrees: BTreeSet<WorktreeKey>,
    expanded_subagents: BTreeSet<SessionKey>,
    hosts: BTreeSet<String>,
    host_filter: Option<String>,
    filter_focus: FocusHandle,
    parent: WeakEntity<Workbench>,
    _tree_subscription: Subscription,
    _filter_focus_subscription: Subscription,
}

impl SessionNavigation {
    pub(super) fn new(
        projection: &WorkbenchProjection,
        selected: Option<&SessionKey>,
        parent: WeakEntity<Workbench>,
        window: &mut Window,
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
            None,
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
        let filter_focus = cx.focus_handle();
        let filter_focus_subscription =
            cx.on_focus_in(&filter_focus, window, |this, window, cx| {
                let focus = this.filter_focus.clone();
                let parent = this.parent.clone();
                window.defer(cx, move |window, cx| {
                    if !focus.contains_focused(window, cx) {
                        return;
                    }
                    if let Some(owner) = parent.upgrade() {
                        owner.update(cx, |owner, cx| owner.host_filter_focused(cx));
                    }
                });
            });
        Self {
            tree,
            targets,
            expansion_targets,
            expanded_projects,
            expanded_worktrees,
            expanded_subagents,
            hosts: collect_hosts(projection),
            host_filter: None,
            filter_focus,
            parent,
            _tree_subscription: tree_subscription,
            _filter_focus_subscription: filter_focus_subscription,
        }
    }

    pub(super) fn tree_state(&self, _: &App) -> Entity<TreeState> {
        self.tree.clone()
    }

    pub(super) fn filter_focus(&self) -> FocusHandle {
        self.filter_focus.clone()
    }

    #[cfg(test)]
    pub(super) fn hosts_changed(&self, projection: &WorkbenchProjection) -> bool {
        self.hosts != collect_hosts(projection)
    }

    fn select_host(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        if self.host_filter == host {
            return;
        }
        self.host_filter = host;
        let owner = self.parent.upgrade().expect("navigation owner is alive");
        let (projection, selected) = {
            let owner = owner.read(cx);
            (
                owner.projection.clone(),
                owner
                    .current_projection()
                    .map(|session| session.key.clone()),
            )
        };
        self.rebuild(&projection, selected.as_ref(), false, cx);
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
        self.hosts = collect_hosts(projection);
        if self
            .host_filter
            .as_ref()
            .is_some_and(|host| !self.hosts.contains(host))
        {
            self.host_filter = None;
        }
        let (items, targets, expansion_targets) = build_items(
            projection,
            self.host_filter.as_deref(),
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
        cx.notify();
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = self.tree.clone();
        let targets = self.targets.clone();
        let tokens = self.tokens(cx);
        let empty = targets.is_empty();
        let owner = self.parent.upgrade().expect("navigation owner is alive");
        let filter_owner = owner.clone();
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
        let filter_listener =
            window.listener_for(&filter_owner, |this, open: &bool, window, cx| {
                this.host_filter_changed(*open, window, cx);
            });
        let hosts = self.hosts.clone();
        let host_filter = self.host_filter.clone();
        let action_context = self.filter_focus.clone();
        let filter = Button::new("host-filter")
            .ghost()
            .label(host_filter.as_deref().unwrap_or("All Hosts"))
            .disabled(hosts.is_empty())
            .track_focus(&self.filter_focus)
            .dropdown_menu(move |menu, _, _| {
                let menu = menu.action_context(action_context.clone()).menu_with_check(
                    "All Hosts",
                    host_filter.is_none(),
                    Box::new(SelectHost(None)),
                );
                hosts.iter().fold(menu, |menu, host| {
                    menu.menu_with_check(
                        host.clone(),
                        host_filter.as_ref() == Some(host),
                        Box::new(SelectHost(Some(host.clone()))),
                    )
                })
            })
            .on_open_change(move |open, window, cx| filter_listener(open, window, cx));
        div()
            .flex_col()
            .size_full()
            .p(tokens.metis.density_gap)
            .on_action(cx.listener(|this, action: &SelectHost, _, cx| {
                this.select_host(action.0.clone(), cx);
            }))
            .child(filter)
            .children(empty.then(|| {
                div()
                    .text_color(tokens.metis.foreground_secondary)
                    .text_size(tokens.metis.metadata_font_size)
                    .child("Offline · no Host or Sessions")
            }))
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

fn collect_hosts(projection: &WorkbenchProjection) -> BTreeSet<String> {
    projection
        .sessions
        .iter()
        .map(|session| session.key.worktree.project.host_id.clone())
        .collect()
}

fn build_items(
    projection: &WorkbenchProjection,
    host_filter: Option<&str>,
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
    groups.retain(|host, _| host_filter.is_none_or(|filter| filter == host.as_str()));
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
            build_items(&projection, None, &projects, &worktrees, &BTreeSet::new());
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
        let (roots, _, _) = build_items(&projection, None, &projects, &worktrees, &subagents);
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

use crate::git_graph::{GitGraph, decode_log_filter, encode_log_filter};
use collections::HashMap;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Subscription, Task,
    WeakEntity, Window, actions, px,
};
use project::git_store::{GitStore, GitStoreEvent};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    git_log_panel,
    [
        /// Toggles focus on the Git Log panel.
        ToggleFocus,
    ]
);

const GIT_LOG_PANEL_KEY: &str = "GitLogPanel";

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<GitLogPanel>(window, cx);
    });
}

/// WebStorm's Git tool window: the git log in a dock, next to the editor.
pub struct GitLogPanel {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    graph: Option<Entity<GitGraph>>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// The latest filter of each repository this session, ahead of the
    /// asynchronous writes to the database.
    filters: HashMap<String, String>,
    /// Database writes run one after another, so an older filter never lands last.
    write_task: Option<Task<()>>,
    _subscription: Subscription,
    _graph_subscription: Option<Subscription>,
}

impl GitLogPanel {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let git_store = workspace.project().read(cx).git_store().clone();
        let workspace = workspace.weak_handle();
        cx.new(|cx| {
            let subscription = cx.subscribe_in(
                &git_store,
                window,
                |this: &mut Self, _, event: &GitStoreEvent, window, cx| {
                    if matches!(event, GitStoreEvent::ActiveRepositoryChanged(_)) {
                        this.follow_active_repository(window, cx);
                    }
                },
            );
            let mut panel = Self {
                workspace,
                git_store,
                graph: None,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Bottom,
                filters: HashMap::default(),
                write_task: None,
                _subscription: subscription,
                _graph_subscription: None,
            };
            panel.follow_active_repository(window, cx);
            panel
        })
    }

    fn follow_active_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo_id) = self
            .git_store
            .read(cx)
            .active_repository()
            .map(|repository| repository.read(cx).id)
        else {
            // The last repository went away: show the empty state, not its old log.
            if self.graph.take().is_some() {
                self._graph_subscription = None;
                cx.notify();
            }
            return;
        };
        let graph = match &self.graph {
            Some(graph) => {
                if graph.read(cx).repo_id() == repo_id {
                    return;
                }
                graph.update(cx, |graph, cx| graph.set_repo_id(repo_id, cx));
                graph.clone()
            }
            None => {
                let git_store = self.git_store.clone();
                let workspace = self.workspace.clone();
                let graph =
                    cx.new(|cx| GitGraph::new(repo_id, git_store, workspace, None, window, cx));
                self._graph_subscription =
                    Some(cx.observe(&graph, |this, _, cx| this.save_filter(cx)));
                self.graph = Some(graph.clone());
                graph
            }
        };
        if let Some(key) = self.filter_key(cx)
            && let Some(filter) = self
                .filters
                .get(&key)
                .cloned()
                .or_else(|| {
                    db::kvp::KeyValueStore::global(cx)
                        .read_kvp(&key)
                        .log_err()
                        .flatten()
                })
                .and_then(|encoded| decode_log_filter(&encoded))
        {
            graph.update(cx, |graph, cx| graph.restore_log_filter(filter, cx));
        }
        cx.notify();
    }

    /// Filters are remembered per repository, like WebStorm's log filters.
    fn filter_key(&self, cx: &App) -> Option<String> {
        let repo_id = self.graph.as_ref()?.read(cx).repo_id();
        let repository = self
            .git_store
            .read(cx)
            .repositories()
            .get(&repo_id)?
            .clone();
        let path = repository.read(cx).work_directory_abs_path.clone();
        Some(format!("evb_git_log_filter:{}", path.display()))
    }

    fn save_filter(&mut self, cx: &mut Context<Self>) {
        let Some(filter) = self
            .graph
            .as_ref()
            .and_then(|graph| graph.read(cx).log_filter_state())
        else {
            return;
        };
        let Some(key) = self.filter_key(cx) else {
            return;
        };
        let encoded = encode_log_filter(&filter);
        if self.filters.get(&key) == Some(&encoded) {
            return;
        }
        self.filters.insert(key.clone(), encoded.clone());
        let kvp = db::kvp::KeyValueStore::global(cx);
        let previous_write = self.write_task.take();
        self.write_task = Some(cx.background_spawn(async move {
            if let Some(previous_write) = previous_write {
                previous_write.await;
            }
            kvp.write_kvp(key, encoded).await.log_err();
        }));
    }

    pub fn graph(&self) -> Option<&Entity<GitGraph>> {
        self.graph.as_ref()
    }
}

impl EventEmitter<PanelEvent> for GitLogPanel {}

impl Focusable for GitLogPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.graph {
            Some(graph) => graph.read(cx).root_focus_handle(),
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for GitLogPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.graph {
            Some(graph) => div().size_full().child(graph.clone()),
            None => div()
                .size_full()
                .track_focus(&self.focus_handle)
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().colors().panel_background)
                .child(Label::new("No Git repository").color(Color::Muted)),
        }
    }
}

impl Panel for GitLogPanel {
    fn persistent_name() -> &'static str {
        "GitLogPanel"
    }

    fn panel_key() -> &'static str {
        GIT_LOG_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Git Log")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        12
    }
}

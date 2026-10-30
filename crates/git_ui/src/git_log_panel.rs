use crate::git_graph::GitGraph;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Subscription,
    WeakEntity, Window, actions, px,
};
use project::git_store::{GitStore, GitStoreEvent};
use ui::prelude::*;
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
    _subscription: Subscription,
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
                _subscription: subscription,
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
            return;
        };
        match &self.graph {
            Some(graph) => graph.update(cx, |graph, cx| graph.set_repo_id(repo_id, cx)),
            None => {
                let git_store = self.git_store.clone();
                let workspace = self.workspace.clone();
                self.graph = Some(
                    cx.new(|cx| GitGraph::new(repo_id, git_store, workspace, None, window, cx)),
                );
            }
        }
        cx.notify();
    }

    pub fn graph(&self) -> Option<&Entity<GitGraph>> {
        self.graph.as_ref()
    }
}

impl EventEmitter<PanelEvent> for GitLogPanel {}

impl Focusable for GitLogPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.graph {
            Some(graph) => graph.focus_handle(cx),
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

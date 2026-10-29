//! JetBrains-style run configurations for EVB IDE.
//!
//! A run configuration is a task template that the title bar widget can run,
//! rerun, stop and debug with one click. Configurations come from the scripts in
//! each visible worktree's `package.json` and `composer.json`, and from the
//! templates in `.zed/tasks.json` and the global tasks file.

use std::{borrow::Cow, path::Path, sync::Arc};

use collections::HashMap;
use dap::adapters::DebugAdapterName;
use fs::Fs;
use gpui::{
    Action, App, AppContext as _, Context, Entity, EntityId, Global, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, Task, TaskExt as _, WeakEntity, Window, actions,
};
use project::{Project, TaskSourceKind, WorktreeId};
use task::{RevealStrategy, TaskContext, TaskId, TaskTemplate};
use terminal::TaskStatus;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use theme::ActiveTheme as _;
use ui::{
    ButtonLike, ContextMenu, Icon, IconButton, IconName, IconSize, IconWithIndicator, Indicator,
    Label, LabelSize, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{Toast, Workspace, notifications::NotificationId};

actions!(
    evb_run,
    [
        /// Runs the selected run configuration, restarting it if it is already running.
        Run,
        /// Starts a debug session for the selected run configuration.
        Debug,
        /// Stops the selected run configuration.
        Stop,
        /// Opens the run configuration selector.
        ChooseConfiguration,
        /// Opens the project tasks file, where custom run configurations live.
        EditConfigurations,
    ]
);

const SELECTED_CONFIGURATION_KEY_PREFIX: &str = "evb_run_selected_configuration:";
const SOURCE_ID_BASE: &str = "evb_run";
const JAVASCRIPT_ADAPTER: &str = "JavaScript";

/// Root files whose changes can add, remove or change run configurations.
const WATCHED_ROOT_FILES: &[&str] = &[
    "package.json",
    "composer.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
];

pub fn init(cx: &mut App) {
    cx.set_global(RunControllers::default());
    cx.observe_new(
        |workspace: &mut Workspace, _: Option<&mut Window>, cx: &mut Context<Workspace>| {
            let workspace_id = cx.entity_id();
            let controller = cx.new(|cx| RunController::new(workspace, cx));
            cx.global_mut::<RunControllers>()
                .0
                .insert(workspace_id, controller.downgrade());
            cx.on_release(move |_, cx| {
                cx.global_mut::<RunControllers>().0.remove(&workspace_id);
            })
            .detach();

            // Workspace action handlers run while the workspace is being updated, and
            // the controller updates the workspace, so its work is deferred.
            workspace
                .register_action({
                    let controller = controller.clone();
                    move |_, _: &Run, window, cx| {
                        let controller = controller.clone();
                        window.defer(cx, move |window, cx| {
                            controller
                                .update(cx, |controller, cx| controller.run_selected(window, cx))
                        });
                    }
                })
                .register_action({
                    let controller = controller.clone();
                    move |_, _: &Debug, window, cx| {
                        let controller = controller.clone();
                        window.defer(cx, move |window, cx| {
                            controller
                                .update(cx, |controller, cx| controller.debug_selected(window, cx))
                        });
                    }
                })
                .register_action({
                    let controller = controller.clone();
                    move |_, _: &Stop, window, cx| {
                        let controller = controller.clone();
                        window.defer(cx, move |_, cx| {
                            controller.update(cx, |controller, cx| controller.stop_selected(cx))
                        });
                    }
                })
                .register_action(move |_, _: &ChooseConfiguration, window, cx| {
                    let handle = controller.read(cx).menu_handle.clone();
                    handle.toggle(window, cx);
                })
                .register_action(move |_, _: &EditConfigurations, window, cx| {
                    window.dispatch_action(zed_actions::OpenProjectTasks.boxed_clone(), cx);
                });
        },
    )
    .detach();
}

#[derive(Default)]
struct RunControllers(HashMap<EntityId, WeakEntity<RunController>>);

impl Global for RunControllers {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl PackageManager {
    pub fn command(self) -> &'static str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn => "yarn",
            PackageManager::Bun => "bun",
        }
    }

    /// Picks the package manager the way the project's lockfile or the
    /// `packageManager` field in `package.json` declares it, falling back to npm.
    pub fn detect(root_files: &[&str], package_json: &serde_json::Value) -> Self {
        let has = |name: &str| root_files.contains(&name);
        if has("pnpm-lock.yaml") {
            return PackageManager::Pnpm;
        }
        if has("yarn.lock") {
            return PackageManager::Yarn;
        }
        if has("bun.lock") || has("bun.lockb") {
            return PackageManager::Bun;
        }
        if has("package-lock.json") {
            return PackageManager::Npm;
        }
        let declared = package_json
            .get("packageManager")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if declared.starts_with("pnpm@") {
            PackageManager::Pnpm
        } else if declared.starts_with("yarn@") {
            PackageManager::Yarn
        } else if declared.starts_with("bun@") {
            PackageManager::Bun
        } else {
            PackageManager::Npm
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunConfigurationKind {
    PackageScript(PackageManager),
    ComposerScript,
    Task,
}

impl RunConfigurationKind {
    fn icon(self) -> IconName {
        match self {
            RunConfigurationKind::PackageScript(_) => IconName::FileTree,
            RunConfigurationKind::ComposerScript => IconName::FileCode,
            RunConfigurationKind::Task => IconName::Terminal,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunConfiguration {
    /// Stable across restarts, used to persist the selection.
    pub id: SharedString,
    pub label: SharedString,
    pub kind: RunConfigurationKind,
    pub worktree_id: Option<WorktreeId>,
    source_kind: TaskSourceKind,
    template: TaskTemplate,
}

/// Script names in declaration order from a `package.json` or `composer.json`.
pub fn script_names(manifest: &serde_json::Value) -> Vec<String> {
    manifest
        .get("scripts")
        .and_then(|scripts| scripts.as_object())
        .map(|scripts| scripts.keys().cloned().collect())
        .unwrap_or_default()
}

/// `label` names the task's terminal. Zed reuses a terminal for tasks with the
/// same label, so scripts of different roots get their root-prefixed label.
fn package_script_template(
    manager: PackageManager,
    script: &str,
    label: &str,
    cwd: &Path,
) -> TaskTemplate {
    TaskTemplate {
        label: label.to_string(),
        command: manager.command().to_string(),
        args: vec!["run".to_string(), script.to_string()],
        cwd: Some(cwd.to_string_lossy().into_owned()),
        reveal: RevealStrategy::Always,
        ..TaskTemplate::default()
    }
}

fn composer_script_template(script: &str, label: &str, cwd: &Path) -> TaskTemplate {
    TaskTemplate {
        label: label.to_string(),
        command: "composer".to_string(),
        args: vec!["run-script".to_string(), script.to_string()],
        cwd: Some(cwd.to_string_lossy().into_owned()),
        reveal: RevealStrategy::Always,
        ..TaskTemplate::default()
    }
}

/// The configuration to select when nothing was selected before: the usual
/// development server script, otherwise the first configuration.
pub fn preferred_configuration(configurations: &[RunConfiguration]) -> Option<SharedString> {
    const PREFERRED_SCRIPTS: &[&str] = &["dev", "start", "serve"];
    PREFERRED_SCRIPTS
        .iter()
        .find_map(|script| {
            configurations.iter().find(|configuration| {
                matches!(configuration.kind, RunConfigurationKind::PackageScript(_))
                    && configuration.template.args.last().map(String::as_str) == Some(*script)
            })
        })
        .or_else(|| configurations.first())
        .map(|configuration| configuration.id.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

struct RunState {
    status: RunStatus,
    /// Identifies the run's terminal; labels can repeat across configurations.
    task_id: TaskId,
    /// Distinguishes a restarted run from the one it replaced, whose completion
    /// arrives after the new run started.
    generation: usize,
}

struct RootManifests {
    worktree_id: WorktreeId,
    root_name: String,
    abs_path: Arc<Path>,
}

pub struct RunController {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    configurations: Vec<RunConfiguration>,
    selected: Option<SharedString>,
    runs: HashMap<SharedString, RunState>,
    /// Launches that have not handed their task to the workspace yet. Dropping
    /// one (on Stop or Rerun) cancels it.
    launches: HashMap<SharedString, Task<()>>,
    next_generation: usize,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    refresh_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl RunController {
    pub fn for_workspace(workspace_id: EntityId, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<RunControllers>()?
            .0
            .get(&workspace_id)?
            .upgrade()
    }

    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let subscription = cx.subscribe(&project, |this, _, event, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                this.refresh(cx)
            }
            project::Event::WorktreeUpdatedEntries(_, entries) => {
                let relevant = entries.iter().any(|(path, _, _)| {
                    path.as_unix_str() == ".zed/tasks.json"
                        || WATCHED_ROOT_FILES.contains(&path.as_unix_str())
                });
                if relevant {
                    this.refresh(cx)
                }
            }
            _ => {}
        });
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            configurations: Vec::new(),
            selected: None,
            runs: HashMap::default(),
            launches: HashMap::default(),
            next_generation: 0,
            menu_handle: PopoverMenuHandle::default(),
            refresh_task: Task::ready(()),
            _subscriptions: vec![subscription],
        };
        this.refresh(cx);
        this
    }

    pub fn configurations(&self) -> &[RunConfiguration] {
        &self.configurations
    }

    pub fn selected_configuration(&self) -> Option<&RunConfiguration> {
        let selected = self.selected.as_ref()?;
        self.configurations
            .iter()
            .find(|configuration| &configuration.id == selected)
    }

    pub fn status(&self, configuration_id: &str) -> Option<RunStatus> {
        self.runs.get(configuration_id).map(|run| run.status)
    }

    fn is_running(&self, configuration_id: &str) -> bool {
        self.status(configuration_id) == Some(RunStatus::Running)
    }

    fn selection_key(&self, cx: &App) -> Option<String> {
        let worktree = self.project.read(cx).visible_worktrees(cx).next()?;
        let abs_path = worktree.read(cx).abs_path();
        Some(format!(
            "{SELECTED_CONFIGURATION_KEY_PREFIX}{}",
            abs_path.display()
        ))
    }

    pub fn select(&mut self, configuration_id: SharedString, cx: &mut Context<Self>) {
        if let Some(key) = self.selection_key(cx) {
            let kvp = db::kvp::KeyValueStore::global(cx);
            let value = configuration_id.to_string();
            cx.background_spawn(async move { kvp.write_kvp(key, value).await })
                .detach_and_log_err(cx);
        }
        self.selected = Some(configuration_id);
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let project = self.project.read(cx);
        let fs = project.fs().clone();
        // `Project::fs` is this machine's file system; manifests of a remote
        // project live on the remote host, so only its tasks.json is used.
        let discover_manifests = project.is_local();
        let roots = project
            .visible_worktrees(cx)
            .filter(|worktree| worktree.read(cx).root_entry().is_some_and(|e| e.is_dir()))
            .map(|worktree| {
                let worktree = worktree.read(cx);
                RootManifests {
                    worktree_id: worktree.id(),
                    root_name: worktree.root_name().as_unix_str().to_string(),
                    abs_path: worktree.abs_path(),
                }
            })
            .collect::<Vec<_>>();

        let inventory = project.task_store().read(cx).task_inventory().cloned();
        // Each worktree's listing also contains the global tasks; they are
        // deduplicated by id below.
        let template_tasks = match &inventory {
            None => Vec::new(),
            Some(inventory) if roots.is_empty() => {
                vec![(None, inventory.read(cx).list_tasks(None, None, None, cx))]
            }
            Some(inventory) => roots
                .iter()
                .map(|root| {
                    (
                        Some(root.worktree_id),
                        inventory
                            .read(cx)
                            .list_tasks(None, None, Some(root.worktree_id), cx),
                    )
                })
                .collect(),
        };

        let saved_selection = self.selection_key(cx).and_then(|key| {
            db::kvp::KeyValueStore::global(cx)
                .read_kvp(&key)
                .log_err()
                .flatten()
        });

        self.refresh_task = cx.spawn(async move |this, cx| {
            let show_root_names = roots.len() > 1;
            let mut configurations = Vec::new();
            for root in roots.iter().filter(|_| discover_manifests) {
                let mut root_files = Vec::new();
                for name in WATCHED_ROOT_FILES {
                    if fs.is_file(&root.abs_path.join(name)).await {
                        root_files.push(*name);
                    }
                }
                let prefix = |script: &str| {
                    if show_root_names {
                        format!("{}: {script}", root.root_name)
                    } else {
                        script.to_string()
                    }
                };
                let source_kind = TaskSourceKind::Worktree {
                    id: root.worktree_id,
                    directory_in_worktree: RelPath::empty_arc(),
                    id_base: Cow::Borrowed(SOURCE_ID_BASE),
                };

                if root_files.contains(&"package.json") {
                    let manifest =
                        load_manifest(fs.as_ref(), &root.abs_path.join("package.json")).await;
                    if let Some(manifest) = manifest {
                        let manager = PackageManager::detect(&root_files, &manifest);
                        for script in script_names(&manifest) {
                            configurations.push(RunConfiguration {
                                id: format!("package:{}:{script}", root.abs_path.display()).into(),
                                label: prefix(&script).into(),
                                kind: RunConfigurationKind::PackageScript(manager),
                                worktree_id: Some(root.worktree_id),
                                source_kind: source_kind.clone(),
                                template: package_script_template(
                                    manager,
                                    &script,
                                    &prefix(&script),
                                    &root.abs_path,
                                ),
                            });
                        }
                    }
                }
                if root_files.contains(&"composer.json") {
                    let manifest =
                        load_manifest(fs.as_ref(), &root.abs_path.join("composer.json")).await;
                    if let Some(manifest) = manifest {
                        for script in script_names(&manifest) {
                            configurations.push(RunConfiguration {
                                id: format!("composer:{}:{script}", root.abs_path.display()).into(),
                                label: prefix(&script).into(),
                                kind: RunConfigurationKind::ComposerScript,
                                worktree_id: Some(root.worktree_id),
                                source_kind: source_kind.clone(),
                                template: composer_script_template(
                                    &script,
                                    &prefix(&script),
                                    &root.abs_path,
                                ),
                            });
                        }
                    }
                }
            }

            let mut seen_task_ids = collections::HashSet::default();
            for (worktree_id, tasks) in template_tasks {
                for (source_kind, template) in tasks.await {
                    // Persisted, so built from paths rather than the per-session
                    // worktree id inside `TaskSourceKind::to_id_base`.
                    let source = match &source_kind {
                        TaskSourceKind::Worktree {
                            id,
                            directory_in_worktree,
                            ..
                        } => roots
                            .iter()
                            .find(|root| root.worktree_id == *id)
                            .map(|root| {
                                format!(
                                    "{}/{}",
                                    root.abs_path.display(),
                                    directory_in_worktree.as_unix_str()
                                )
                            })
                            .unwrap_or_else(|| source_kind.to_id_base()),
                        TaskSourceKind::AbsPath { abs_path, .. } => abs_path.display().to_string(),
                        _ => source_kind.to_id_base(),
                    };
                    let id: SharedString = format!("task:{source}:{}", template.label).into();
                    if !seen_task_ids.insert(id.clone()) {
                        continue;
                    }
                    // Global tasks belong to no worktree; they run in the active
                    // file's context.
                    let worktree_id = match source_kind {
                        TaskSourceKind::AbsPath { .. } => None,
                        _ => worktree_id,
                    };
                    configurations.push(RunConfiguration {
                        id,
                        label: template.label.clone().into(),
                        kind: RunConfigurationKind::Task,
                        worktree_id,
                        source_kind,
                        template,
                    });
                }
            }

            this.update(cx, |this, cx| {
                this.configurations = configurations;
                let selection_is_valid = |id: &str| {
                    this.configurations
                        .iter()
                        .any(|configuration| configuration.id.as_ref() == id)
                };
                let current = this.selected.clone().filter(|id| selection_is_valid(id));
                let saved = saved_selection
                    .filter(|id| selection_is_valid(id))
                    .map(SharedString::from);
                this.selected = current
                    .or(saved)
                    .or_else(|| preferred_configuration(&this.configurations));
                cx.notify();
            })
            .log_err();
        });
    }

    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(configuration) = self.selected_configuration().cloned() else {
            self.menu_handle.show(window, cx);
            return;
        };
        self.run(configuration, window, cx);
    }

    pub fn run(
        &mut self,
        configuration: RunConfiguration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_running(&configuration.id) {
            self.stop(&configuration.id, cx);
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let task_contexts = workspace.update(cx, |workspace, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        });
        let generation = self.next_generation;
        self.next_generation += 1;

        let configuration_id = configuration.id.clone();
        let launch = cx.spawn_in(window, async move |this, cx| {
            let task_contexts = task_contexts.await;
            let task_context = task_context_for(&configuration, &task_contexts)
                .cloned()
                .unwrap_or_else(TaskContext::default);
            let Some(resolved) = configuration
                .template
                .resolve_task(&configuration.source_kind.to_id_base(), &task_context)
            else {
                log::warn!(
                    "could not resolve run configuration {}",
                    configuration.label
                );
                return;
            };

            this.update(cx, |this, cx| {
                this.runs.insert(
                    configuration.id.clone(),
                    RunState {
                        status: RunStatus::Running,
                        task_id: resolved.id.clone(),
                        generation,
                    },
                );
                cx.notify();
            })
            .log_err();

            let configuration_id = configuration.id.clone();
            let this_for_completion = this.clone();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.schedule_resolved_task_with_completion(
                        configuration.source_kind.clone(),
                        resolved,
                        false,
                        move |result, cx| {
                            this_for_completion
                                .update(cx, |this, cx| {
                                    let Some(run) = this.runs.get_mut(&configuration_id) else {
                                        return;
                                    };
                                    if run.generation != generation {
                                        return;
                                    }
                                    run.status = match result {
                                        workspace::tasks::ScheduledTaskResult::Success => {
                                            RunStatus::Succeeded
                                        }
                                        _ => RunStatus::Failed,
                                    };
                                    cx.notify();
                                })
                                .log_err();
                        },
                        window,
                        cx,
                    )
                })
                .log_err();
        });
        self.launches.insert(configuration_id, launch);
    }

    fn stop_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected.clone() {
            self.stop(&selected, cx);
        }
    }

    /// Stops the configuration's running process and cancels a launch that
    /// has not reached the workspace yet. A task whose launch is already saving
    /// files before spawning (`"save": "all"`) still starts and needs a second Stop.
    pub fn stop(&mut self, configuration_id: &str, cx: &mut Context<Self>) {
        self.launches.remove(configuration_id);
        let Some(run) = self.runs.get(configuration_id) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        for terminal in running_task_terminals(&workspace, &run.task_id, cx) {
            terminal.update(cx, |terminal, _| terminal.kill_active_task());
        }
    }

    fn debug_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(configuration) = self.selected_configuration().cloned() else {
            self.menu_handle.show(window, cx);
            return;
        };
        self.debug(configuration, window, cx);
    }

    pub fn debug(
        &mut self,
        configuration: RunConfiguration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let dap_store = self.project.read(cx).dap_store();
        let scenario = dap_store.update(cx, |dap_store, cx| {
            dap_store.debug_scenario_for_build_task(
                configuration.template.clone(),
                DebugAdapterName(JAVASCRIPT_ADAPTER.into()),
                configuration.label.clone(),
                cx,
            )
        });
        let task_contexts = workspace.update(cx, |workspace, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        });

        cx.spawn_in(window, async move |_, cx| {
            let scenario = scenario.await;
            let task_contexts = task_contexts.await;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let Some(scenario) = scenario else {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<RunController>(),
                                format!(
                                    "No debugger can launch '{}'. Add a configuration to .zed/debug.json.",
                                    configuration.label
                                ),
                            ),
                            cx,
                        );
                        return;
                    };
                    let task_context = configuration
                        .worktree_id
                        .and_then(|worktree_id| {
                            task_contexts.task_context_for_worktree_id(worktree_id)
                        })
                        .cloned()
                        .unwrap_or_default();
                    workspace.start_debug_session(
                        scenario,
                        task_context.into(),
                        None,
                        configuration.worktree_id,
                        window,
                        cx,
                    );
                })
                .log_err();
        })
        .detach();
    }

    fn build_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        self.refresh(cx);
        let this = cx.weak_entity();
        let configurations = self.configurations.clone();
        let selected = self.selected.clone();
        let statuses = configurations
            .iter()
            .map(|configuration| self.status(&configuration.id))
            .collect::<Vec<_>>();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            if configurations.is_empty() {
                menu = menu.header("No run configurations found");
            }
            let mut previous_kind = None;
            for (configuration, status) in configurations.iter().zip(statuses) {
                let kind_header = match configuration.kind {
                    RunConfigurationKind::PackageScript(manager) => {
                        format!("{} scripts", manager.command())
                    }
                    RunConfigurationKind::ComposerScript => "composer scripts".to_string(),
                    RunConfigurationKind::Task => "Tasks".to_string(),
                };
                if previous_kind.as_ref() != Some(&kind_header) {
                    if previous_kind.is_some() {
                        menu = menu.separator();
                    }
                    menu = menu.header(kind_header.clone());
                    previous_kind = Some(kind_header);
                }
                let label = match status {
                    Some(RunStatus::Running) => format!("{}  (running)", configuration.label),
                    _ => configuration.label.to_string(),
                };
                let id = configuration.id.clone();
                let this = this.clone();
                menu = menu.toggleable_entry(
                    label,
                    selected.as_ref() == Some(&configuration.id),
                    ui::IconPosition::Start,
                    None,
                    move |_, cx| {
                        this.update(cx, |this, cx| this.select(id.clone(), cx))
                            .log_err();
                    },
                );
            }
            menu.separator()
                .action("Edit Configurations…", EditConfigurations.boxed_clone())
        })
    }
}

/// The context a configuration's variables resolve against: the active file's
/// when it belongs to the configuration's worktree (or the configuration is a
/// global task), so task templates can use `$ZED_FILE` and friends, otherwise
/// the worktree root's.
fn task_context_for<'a>(
    configuration: &RunConfiguration,
    task_contexts: &'a project::TaskContexts,
) -> Option<&'a TaskContext> {
    let active_item_context = task_contexts
        .active_item_context
        .as_ref()
        .filter(|(worktree_id, _, _)| {
            configuration.worktree_id.is_none() || *worktree_id == configuration.worktree_id
        })
        .map(|(_, _, context)| context);
    let worktree_context = configuration
        .worktree_id
        .and_then(|worktree_id| task_contexts.task_context_for_worktree_id(worktree_id));
    match configuration.kind {
        RunConfigurationKind::Task => active_item_context.or(worktree_context),
        RunConfigurationKind::PackageScript(_) | RunConfigurationKind::ComposerScript => {
            worktree_context.or(active_item_context)
        }
    }
    .or_else(|| task_contexts.active_context())
}

async fn load_manifest(fs: &dyn Fs, path: &Path) -> Option<serde_json::Value> {
    let text = fs.load(path).await.log_err()?;
    serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))
        .log_err()
}

/// Terminals, in the center panes and the terminal panel, that are still running
/// the given task.
fn running_task_terminals(
    workspace: &Entity<Workspace>,
    task_id: &TaskId,
    cx: &App,
) -> Vec<Entity<terminal::Terminal>> {
    let workspace = workspace.read(cx);
    let mut panes = workspace.panes().to_vec();
    if let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) {
        panes.extend(terminal_panel.read(cx).panes().into_iter().cloned());
    }
    panes
        .iter()
        .flat_map(|pane| {
            pane.read(cx)
                .items()
                .filter_map(|item| item.act_as::<TerminalView>(cx))
                .collect::<Vec<_>>()
        })
        .map(|terminal_view| terminal_view.read(cx).terminal().clone())
        .filter(|terminal| {
            terminal.read(cx).task().is_some_and(|task| {
                task.status == TaskStatus::Running && &task.spawned_task.id == task_id
            })
        })
        .collect()
}

impl Render for RunController {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_configuration().cloned();
        let running = selected
            .as_ref()
            .is_some_and(|configuration| self.is_running(&configuration.id));
        let can_debug = selected.as_ref().is_some_and(|configuration| {
            matches!(
                configuration.kind,
                RunConfigurationKind::PackageScript(
                    PackageManager::Npm | PackageManager::Pnpm | PackageManager::Yarn
                )
            )
        });
        let any_running = self
            .runs
            .values()
            .any(|run| run.status == RunStatus::Running);

        let label: SharedString = selected
            .as_ref()
            .map(|configuration| configuration.label.clone())
            .unwrap_or_else(|| "Current File".into());
        let icon = selected
            .as_ref()
            .map(|configuration| configuration.kind.icon())
            .unwrap_or(IconName::Terminal);
        let this = cx.weak_entity();
        let run_title = match (&selected, running) {
            (Some(configuration), true) => format!("Rerun '{}'", configuration.label),
            (Some(configuration), false) => format!("Run '{}'", configuration.label),
            (None, _) => "Run…".to_string(),
        };
        let debug_title = selected
            .as_ref()
            .map(|configuration| format!("Debug '{}'", configuration.label))
            .unwrap_or_else(|| "Debug…".to_string());
        let stop_title = selected
            .as_ref()
            .map(|configuration| format!("Stop '{}'", configuration.label))
            .unwrap_or_else(|| "Stop".to_string());

        h_flex()
            .id("evb-run-widget")
            .gap_0p5()
            .child(
                PopoverMenu::new("evb-run-configurations")
                    .menu(move |window, cx| {
                        this.upgrade()
                            .map(|this| this.update(cx, |this, cx| this.build_menu(window, cx)))
                    })
                    .trigger_with_tooltip(
                        ButtonLike::new("evb-run-configuration").child(
                            h_flex()
                                .gap_1()
                                .child(
                                    IconWithIndicator::new(
                                        Icon::new(icon).size(IconSize::Small).color(Color::Muted),
                                        any_running.then(|| Indicator::dot().color(Color::Success)),
                                    )
                                    .indicator_border_color(
                                        Some(cx.theme().colors().title_bar_background),
                                    ),
                                )
                                .child(Label::new(label).size(LabelSize::Small).truncate())
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                        Tooltip::for_action_title("Run/Debug Configurations", &ChooseConfiguration),
                    )
                    .with_handle(self.menu_handle.clone())
                    .anchor(gpui::Anchor::TopRight),
            )
            .child(
                IconButton::new(
                    "evb-run",
                    if running {
                        IconName::Rerun
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Small)
                .icon_color(Color::Success)
                .tooltip(Tooltip::for_action_title(run_title, &Run))
                .on_click(|_, window, cx| window.dispatch_action(Run.boxed_clone(), cx)),
            )
            .child(
                IconButton::new("evb-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Success)
                    .disabled(!can_debug)
                    .tooltip(Tooltip::for_action_title(debug_title, &Debug))
                    .on_click(|_, window, cx| window.dispatch_action(Debug.boxed_clone(), cx)),
            )
            .child(
                IconButton::new("evb-stop", IconName::Stop)
                    .icon_size(IconSize::Small)
                    .icon_color(if running { Color::Error } else { Color::Muted })
                    .disabled(!running)
                    .tooltip(Tooltip::for_action_title(stop_title, &Stop))
                    .on_click(|_, window, cx| window.dispatch_action(Stop.boxed_clone(), cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    async fn discovers_package_and_composer_scripts(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            project::task_store::TaskStore::init(None);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/trailmap"),
            json!({
                "package.json": r#"{ "scripts": { "build": "nuxt build", "dev": "nuxt dev" } }"#,
                "pnpm-lock.yaml": "",
                "composer.json": r#"{ "scripts": { "test": "phpunit" } }"#,
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/trailmap").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let controller = cx
            .update(|_, cx| RunController::for_workspace(workspace.entity_id(), cx))
            .expect("every workspace gets a run controller");
        controller.read_with(cx, |controller, _| {
            let summary = controller
                .configurations()
                .iter()
                .map(|configuration| {
                    format!(
                        "{} {} {}",
                        configuration.label,
                        configuration.template.command,
                        configuration.template.args.join(" ")
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                summary,
                vec![
                    "build pnpm run build",
                    "dev pnpm run dev",
                    "test composer run-script test",
                ]
            );
            assert_eq!(
                controller
                    .selected_configuration()
                    .map(|configuration| configuration.label.clone()),
                Some("dev".into())
            );
        });
    }

    #[gpui::test]
    async fn keeps_same_named_scripts_of_different_roots_apart(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            project::task_store::TaskStore::init(None);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/work"),
            json!({
                "web": {
                    "package.json": r#"{ "scripts": { "dev": "vite" } }"#,
                    "yarn.lock": "",
                },
                "api": {
                    "package.json": r#"{ "scripts": { "dev": "nest start" } }"#,
                    ".zed": { "tasks.json": r#"[{ "label": "migrate", "command": "make" }]"# },
                },
            }),
        )
        .await;
        let project = Project::test(
            fs,
            [path!("/work/web").as_ref(), path!("/work/api").as_ref()],
            cx,
        )
        .await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let controller = cx
            .update(|_, cx| RunController::for_workspace(workspace.entity_id(), cx))
            .expect("every workspace gets a run controller");
        controller.read_with(cx, |controller, _| {
            let scripts = controller
                .configurations()
                .iter()
                .filter(|configuration| configuration.kind != RunConfigurationKind::Task)
                .map(|configuration| {
                    (
                        configuration.label.to_string(),
                        configuration.template.label.clone(),
                        configuration.template.command.clone(),
                    )
                })
                .collect::<Vec<_>>();
            // Terminals are matched by task label, so the labels must differ.
            assert_eq!(
                scripts,
                vec![
                    (
                        "web: dev".to_string(),
                        "web: dev".to_string(),
                        "yarn".to_string()
                    ),
                    (
                        "api: dev".to_string(),
                        "api: dev".to_string(),
                        "npm".to_string()
                    ),
                ]
            );
            let migrate = controller
                .configurations()
                .iter()
                .find(|configuration| configuration.label.as_ref() == "migrate")
                .expect("the worktree tasks.json task is a configuration");
            // Persisted ids must survive reopening, when worktree ids change.
            assert!(
                migrate.id.contains(path!("/work/api")),
                "unstable id {}",
                migrate.id
            );
        });
    }

    #[test]
    fn detects_package_manager_from_lockfiles_then_manifest() {
        let manifest = json!({ "packageManager": "yarn@4.1.0" });
        assert_eq!(
            PackageManager::detect(&["package.json", "pnpm-lock.yaml"], &manifest),
            PackageManager::Pnpm
        );
        assert_eq!(
            PackageManager::detect(&["package.json", "bun.lockb"], &json!({})),
            PackageManager::Bun
        );
        assert_eq!(
            PackageManager::detect(&["package.json"], &manifest),
            PackageManager::Yarn
        );
        assert_eq!(
            PackageManager::detect(&["package.json"], &json!({})),
            PackageManager::Npm
        );
    }

    #[test]
    fn script_names_keep_declaration_order() {
        let manifest = json!({ "scripts": { "dev": "nuxt dev", "build": "nuxt build", "a": "x" } });
        assert_eq!(script_names(&manifest), vec!["dev", "build", "a"]);
        assert!(script_names(&json!({ "name": "x" })).is_empty());
    }

    #[test]
    fn package_script_runs_through_the_detected_manager() {
        let template = package_script_template(
            PackageManager::Pnpm,
            "package:mac",
            "package:mac",
            Path::new("/p"),
        );
        assert_eq!(template.command, "pnpm");
        assert_eq!(template.args, vec!["run", "package:mac"]);
        assert_eq!(template.cwd.as_deref(), Some("/p"));
        assert_eq!(template.label, "package:mac");
    }

    #[test]
    fn prefers_dev_script_over_first_configuration() {
        let configuration = |id: &str, kind| RunConfiguration {
            id: id.to_string().into(),
            label: id.to_string().into(),
            kind,
            worktree_id: None,
            source_kind: TaskSourceKind::UserInput,
            template: TaskTemplate {
                label: id.to_string(),
                args: vec!["run".to_string(), id.to_string()],
                ..TaskTemplate::default()
            },
        };
        let npm = RunConfigurationKind::PackageScript(PackageManager::Npm);
        let configurations = vec![
            configuration("build", npm),
            configuration("dev", npm),
            configuration("lint", RunConfigurationKind::Task),
        ];
        assert_eq!(
            preferred_configuration(&configurations).as_deref(),
            Some("dev")
        );
        assert_eq!(
            preferred_configuration(&configurations[..1]).as_deref(),
            Some("build")
        );
        assert_eq!(preferred_configuration(&[]), None);
    }
}

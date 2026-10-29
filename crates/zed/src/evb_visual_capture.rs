// Allow blocking process commands in this binary - it's a synchronous capture tool
#![allow(clippy::disallowed_methods)]

//! EVB IDE visual capture.
//!
//! Renders EVB IDE features in hidden, unfocused windows and saves PNG
//! screenshots, using the same `VisualTestAppContext` as Zed's visual test
//! runner. Unlike that runner it compares nothing: it produces evidence images
//! for review.
//!
//!   cargo run -p zed --bin evb-visual-capture --features visual-tests
//!
//! Screenshots go to `EVB_CAPTURE_DIR` (default `target/evb_captures`).

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("EVB visual capture is only supported on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    // SAFETY: at the start of main(), before any threads are spawned.
    unsafe {
        std::env::set_var("ZED_STATELESS", "1");
    }
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .init();

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let root = temp_dir
        .keep()
        .canonicalize()
        .expect("Failed to canonicalize temp directory");
    let project_path = root.join("trailmap");
    if let Err(error) = create_fixture_project(&project_path) {
        eprintln!("Failed to create the fixture project: {error:#}");
        std::process::exit(1);
    }

    match std::panic::catch_unwind(|| capture::run(project_path)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("Capture failed: {error:#}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("Capture panicked");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn create_fixture_project(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::process::Command;

    let files: &[(&str, &str)] = &[
        (
            "package.json",
            r#"{
  "name": "trailmap",
  "private": true,
  "scripts": {
    "dev": "node -e \"console.log('Nuxt dev server ready on http://localhost:3000'); setInterval(() => {}, 1000)\"",
    "build": "node -e \"console.log('built')\"",
    "package:mac": "node -e \"console.log('packaging')\"",
    "lint": "node -e \"console.log('lint ok')\""
  }
}
"#,
        ),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        (
            "server/api/trip.get.ts",
            r#"import { getTripStore } from '../utils/trip-store'
import { requireViewer } from '../utils/session'
import { toTripSummary } from '../utils/trip-summary'

// Itineraries can hold hundreds of stops: map tiles load separately, so this
// response stays small and is never cached across viewers.
export default defineEventHandler(async (event) => {
  const viewer = await requireViewer(event)
  const { id } = getQuery(event)
  const trip = await getTripStore(event).find(String(id), viewer)
  if (!trip) {
    throw createError({ statusCode: 404, statusMessage: 'Trip not found' })
  }
  setResponseHeaders(event, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'private, no-store',
  })
  return toTripSummary(trip)
})
"#,
        ),
        (
            "app/components/AppBar.vue",
            r#"<template>
  <header class="app-bar">
    <span class="app-bar-name">Trailmap</span>
    <UButton :loading="pending" @click="refresh">Refresh</UButton>
  </header>
</template>

<script setup lang="ts">
const { pending, refresh } = await useFetch('/api/trip')
</script>
"#,
        ),
        (
            "README.md",
            "# Trailmap\n\nFixture for EVB IDE captures.\n",
        ),
    ];
    for (relative, contents) in files {
        let file = path.join(relative);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, contents)?;
    }

    let git = |args: &[&str]| -> anyhow::Result<()> {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "Alex Chen")
            .env("GIT_AUTHOR_EMAIL", "alex@example.com")
            .env("GIT_COMMITTER_NAME", "Alex Chen")
            .env("GIT_COMMITTER_EMAIL", "alex@example.com")
            .status()
            .with_context(|| format!("running git {args:?}"))?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
        Ok(())
    };
    git(&["init", "-q", "-b", "master"])?;
    git(&["add", "."])?;
    git(&[
        "commit",
        "-q",
        "-m",
        "feat: add shareable trip links",
    ])?;
    git(&["checkout", "-q", "-b", "fix/map-tiles"])?;
    std::fs::write(path.join("README.md"), "# Trailmap\n\nCI caches the tile fixtures.\n")?;
    git(&[
        "commit",
        "-q",
        "-am",
        "ci: cache the map tile fixtures",
    ])?;
    git(&["checkout", "-q", "master"])?;
    std::fs::write(path.join("README.md"), "# Trailmap\n\nPlan trips on maps shared with friends.\n")?;
    git(&[
        "commit",
        "-q",
        "-am",
        "fix: return 404 for trips the viewer cannot see",
    ])?;
    git(&[
        "merge",
        "-q",
        "--no-ff",
        "--no-edit",
        "fix/map-tiles",
        "-X",
        "theirs",
    ])?;

    // Leave an uncommitted change for the Commit panel.
    let api = path.join("server/api/trip.get.ts");
    let text = std::fs::read_to_string(&api)?;
    std::fs::write(
        &api,
        text.replace(
            "    'cache-control': 'private, no-store',\n",
            "    'cache-control': 'private, no-store',\n    'x-content-type-options': 'nosniff',\n",
        ),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod capture {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use anyhow::{Context as _, Result};
    use assets::Assets;
    use gpui::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, UpdateGlobal as _, VisualTestAppContext,
        WindowBounds, WindowHandle, WindowOptions, point, px, size,
    };
    use project::Project;
    use settings::SettingsStore;
    use util::ResultExt as _;
    use workspace::{AppState, MultiWorkspace, Workspace};

    pub fn run(project_path: PathBuf) -> Result<()> {
        let mut cx = VisualTestAppContext::with_asset_source(
            gpui_platform::current_platform(false),
            Arc::new(Assets),
        );
        cx.update(|cx| {
            Assets.load_fonts(cx).log_err();
            settings::init(cx);
        });
        let app_state = cx.update(init_app_state);
        cx.update(|cx| {
            gpui_tokio::init(cx);
            theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
            client::init(&app_state.client, cx);
            workspace::init(app_state.clone(), cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            command_palette::init(cx);
            editor::init(cx);
            call::init(app_state.client.clone(), app_state.user_store.clone(), cx);
            title_bar::init(cx);
            project_panel::init(cx);
            terminal_view::init(cx);
            search::init(cx);
            language_model::init(cx);
            git_ui::init(cx);
            tasks_ui::init(cx);
            evb_run::init(cx);
            project::debugger::breakpoint_store::BreakpointStore::init(
                &app_state.client.clone().into(),
            );
            project::debugger::dap_store::DapStore::init(&app_state.client.clone().into(), cx);
            if let Some(bindings) = settings::KeymapFile::load_asset(
                settings::EVB_KEYMAP_PATH,
                Some(settings::KeybindSource::Base),
                cx,
            )
            .log_err()
            {
                cx.bind_keys(bindings);
            }
        });
        cx.update(|cx| {
            use theme::ActiveTheme as _;
            app_state.languages.set_theme(cx.theme().clone());
            cx.observe_global::<theme::GlobalTheme>({
                let languages = app_state.languages.clone();
                move |cx| languages.set_theme(cx.theme().clone())
            })
            .detach();
        });
        cx.run_until_parked();

        let output_dir = PathBuf::from(
            std::env::var("EVB_CAPTURE_DIR").unwrap_or_else(|_| "target/evb_captures".into()),
        );
        std::fs::create_dir_all(&output_dir)?;

        let project = create_project(&project_path, &app_state, &mut cx)?;
        let window = open_window(project, &app_state, &mut cx)?;
        let any_window: AnyWindowHandle = window.into();
        let workspace = window
            .update(&mut cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .context("reading the workspace")?;

        load_panels(&workspace, any_window, &mut cx)?;
        open_file(
            &workspace,
            any_window,
            "server/api/trip.get.ts",
            &mut cx,
        )?;
        settle(&mut cx, 20);
        save(&mut cx, any_window, &output_dir, "01-workspace")?;

        cx.dispatch_action(any_window, evb_run::ChooseConfiguration);
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "02-run-configurations")?;
        cx.dispatch_action(any_window, evb_run::ChooseConfiguration);
        settle(&mut cx, 3);

        cx.dispatch_action(any_window, evb_run::Run);
        wait_real_time(&mut cx, Duration::from_secs(4));
        save(&mut cx, any_window, &output_dir, "03-running")?;

        cx.dispatch_action(any_window, evb_run::Stop);
        wait_real_time(&mut cx, Duration::from_secs(2));
        save(&mut cx, any_window, &output_dir, "04-stopped")?;

        workspace
            .update_in_window(any_window, &mut cx, |workspace, window, cx| {
                workspace.close_panel::<terminal_view::terminal_panel::TerminalPanel>(window, cx);
                workspace.open_panel::<git_ui::git_panel::GitPanel>(window, cx);
            })
            .log_err();
        wait_real_time(&mut cx, Duration::from_secs(2));
        save(&mut cx, any_window, &output_dir, "05-commit-panel")?;

        cx.dispatch_action(any_window, git_ui::git_graph::Open);
        wait_real_time(&mut cx, Duration::from_secs(3));
        save(&mut cx, any_window, &output_dir, "06-git-log")?;

        set_theme(&mut cx, "IntelliJ Dark");
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "07-git-log-dark")?;

        open_file(&workspace, any_window, "app/components/AppBar.vue", &mut cx)?;
        wait_real_time(&mut cx, Duration::from_secs(2));
        save(&mut cx, any_window, &output_dir, "08-vue-dark")?;

        set_theme(&mut cx, "IntelliJ Light");
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "09-vue-light")?;

        println!("Captures written to {}", output_dir.display());
        Ok(())
    }

    trait UpdateInWindow {
        fn update_in_window<R>(
            &self,
            window: AnyWindowHandle,
            cx: &mut VisualTestAppContext,
            f: impl FnOnce(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>) -> R,
        ) -> Result<R>;
    }

    impl UpdateInWindow for Entity<Workspace> {
        fn update_in_window<R>(
            &self,
            window: AnyWindowHandle,
            cx: &mut VisualTestAppContext,
            f: impl FnOnce(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>) -> R,
        ) -> Result<R> {
            let workspace = self.clone();
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| f(workspace, window, cx))
            })
        }
    }

    fn set_theme(cx: &mut VisualTestAppContext, name: &str) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.theme = Some(settings::ThemeSelection::Static(
                        settings::ThemeName(name.into()),
                    ));
                });
            });
        });
    }

    fn settle(cx: &mut VisualTestAppContext, ticks: usize) {
        for _ in 0..ticks {
            cx.advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
        }
    }

    /// Lets real processes (task terminals, git) make progress while pumping the
    /// deterministic dispatcher.
    fn wait_real_time(cx: &mut VisualTestAppContext, duration: Duration) {
        let started = std::time::Instant::now();
        while started.elapsed() < duration {
            std::thread::sleep(Duration::from_millis(50));
            cx.advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
        }
    }

    fn save(
        cx: &mut VisualTestAppContext,
        window: AnyWindowHandle,
        output_dir: &Path,
        name: &str,
    ) -> Result<()> {
        cx.update_window(window, |_, window, _| window.refresh())?;
        cx.run_until_parked();
        let screenshot = cx.capture_screenshot(window)?;
        let path = output_dir.join(format!("{name}.png"));
        screenshot.save(&path)?;
        println!("saved {}", path.display());
        Ok(())
    }

    fn init_app_state(cx: &mut gpui::App) -> Arc<AppState> {
        use fs::Fs;
        use node_runtime::NodeRuntime;
        use session::Session;

        let fs: Arc<dyn Fs> = fs::RealFs::new(None, cx.background_executor().clone());
        <dyn Fs>::set_global(fs.clone(), cx);
        let languages = Arc::new(language::LanguageRegistry::new(
            cx.background_executor().clone(),
        ));
        let clock = Arc::new(clock::FakeSystemClock::new());
        let http_client = http_client::FakeHttpClient::with_404_response();
        let client = client::Client::new(clock, http_client, cx);
        let session = cx.new(|cx| session::AppSession::new(Session::test(), cx));
        let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| workspace::WorkspaceStore::new(client.clone(), cx));
        languages::init(
            languages.clone(),
            fs.clone(),
            NodeRuntime::unavailable(),
            cx,
        );

        let app_state = Arc::new(AppState {
            client,
            fs,
            languages,
            user_store,
            workspace_store,
            node_runtime: NodeRuntime::unavailable(),
            build_window_options: |_, _| Default::default(),
            session,
        });
        AppState::set_global(app_state.clone(), cx);
        app_state
    }

    fn create_project(
        path: &Path,
        app_state: &Arc<AppState>,
        cx: &mut VisualTestAppContext,
    ) -> Result<Entity<Project>> {
        let project = cx.update(|cx| {
            Project::local(
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                project::LocalProjectFlags {
                    init_worktree_trust: false,
                    ..Default::default()
                },
                cx,
            )
        });
        let add_worktree = cx.update(|cx| {
            project.update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            })
        });
        cx.background_executor.allow_parking();
        let result = cx.foreground_executor.block_test(add_worktree);
        cx.background_executor.forbid_parking();
        result.context("adding the fixture worktree")?;
        cx.run_until_parked();
        Ok(project)
    }

    fn open_window(
        project: Entity<Project>,
        app_state: &Arc<AppState>,
        cx: &mut VisualTestAppContext,
    ) -> Result<WindowHandle<MultiWorkspace>> {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1440.0), px(900.0)),
        };
        let window = cx.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                |window, cx| {
                    let workspace = cx.new(|cx| {
                        Workspace::new(None, project.clone(), app_state.clone(), window, cx)
                    });
                    cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                },
            )
        })?;
        cx.run_until_parked();
        Ok(window)
    }

    fn load_panels(
        workspace: &Entity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut VisualTestAppContext,
    ) -> Result<()> {
        let (weak_workspace, async_cx) =
            workspace.update_in_window(window, cx, |workspace, window, cx| {
                (workspace.weak_handle(), window.to_async(cx))
            })?;
        cx.background_executor.allow_parking();
        let project_panel = cx
            .foreground_executor
            .block_test(project_panel::ProjectPanel::load(
                weak_workspace.clone(),
                async_cx.clone(),
            ));
        let terminal_panel =
            cx.foreground_executor
                .block_test(terminal_view::terminal_panel::TerminalPanel::load(
                    weak_workspace.clone(),
                    async_cx.clone(),
                ));
        let git_panel = cx
            .foreground_executor
            .block_test(git_ui::git_panel::GitPanel::load(weak_workspace, async_cx));
        cx.background_executor.forbid_parking();
        let (project_panel, terminal_panel, git_panel) = (
            project_panel.context("loading the project panel")?,
            terminal_panel.context("loading the terminal panel")?,
            git_panel.context("loading the git panel")?,
        );
        workspace.update_in_window(window, cx, |workspace, window, cx| {
            workspace.add_panel(project_panel, window, cx);
            workspace.add_panel(terminal_panel, window, cx);
            workspace.add_panel(git_panel, window, cx);
            workspace.open_panel::<project_panel::ProjectPanel>(window, cx);
        })?;
        cx.run_until_parked();
        Ok(())
    }

    fn open_file(
        workspace: &Entity<Workspace>,
        window: AnyWindowHandle,
        relative_path: &str,
        cx: &mut VisualTestAppContext,
    ) -> Result<()> {
        let task = workspace.update_in_window(window, cx, |workspace, window, cx| {
            let worktree = workspace.project().read(cx).worktrees(cx).next()?;
            let worktree_id = worktree.read(cx).id();
            let rel_path: Arc<util::rel_path::RelPath> =
                util::rel_path::rel_path(relative_path).into();
            Some(workspace.open_path((worktree_id, rel_path), None, true, window, cx))
        })?;
        if let Some(task) = task {
            cx.background_executor.allow_parking();
            let result = cx.foreground_executor.block_test(task);
            cx.background_executor.forbid_parking();
            result.context("opening the fixture file")?;
        }
        cx.run_until_parked();
        Ok(())
    }
}

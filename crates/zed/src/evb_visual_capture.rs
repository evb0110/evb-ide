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

    // Everything in the fixture is invented: the frames are published on the landing page.
    let files: &[(&str, &str)] = &[
        (
            "package.json",
            r#"{
  "name": "trailmap",
  "private": true,
  "scripts": {
    "dev": "node -e \"console.log('Nuxt dev server ready on http://localhost:3000'); setInterval(() => {}, 1000)\"",
    "build": "node -e \"console.log('built')\"",
    "preview": "node -e \"console.log('preview')\"",
    "lint": "node -e \"console.log('lint ok')\""
  }
}
"#,
        ),
        // npm, unlike pnpm, does not print the project's absolute path when it runs a script.
        (
            "package-lock.json",
            "{\n  \"name\": \"trailmap\",\n  \"lockfileVersion\": 3,\n  \"requires\": true,\n  \"packages\": {}\n}\n",
        ),
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
const { pending, refresh } = await useFetch('/api/trip', { query: { id: 'alps' } })
</script>
"#,
        ),
        ("README.md", "# Trailmap\n\nPlan trips on shared maps.\n"),
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
    // The git log's "me" comes from the repository's identity, not the global one.
    git(&["config", "user.name", "Alex Chen"])?;
    git(&["config", "user.email", "alex@example.com"])?;
    git(&["add", "."])?;
    git(&["commit", "-q", "-m", "feat: add shareable trip links"])?;
    git(&["checkout", "-q", "-b", "fix/map-tiles"])?;
    std::fs::write(
        path.join("README.md"),
        "# Trailmap\n\nPlan trips on shared maps.\n\nCI caches the tile fixtures.\n",
    )?;
    git(&["commit", "-q", "-am", "ci: cache the map tile fixtures"])?;
    git(&["checkout", "-q", "master"])?;
    std::fs::write(
        path.join("README.md"),
        "# Trailmap\n\nPlan trips on maps shared with friends.\n",
    )?;
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
    // More refs for the branches tree: folders and a remote.
    git(&["branch", "feature/trip-export", "HEAD~1"])?;
    git(&["branch", "feature/offline-maps", "HEAD~2"])?;
    git(&["update-ref", "refs/remotes/origin/master", "HEAD~1"])?;
    git(&[
        "update-ref",
        "refs/remotes/origin/chore/deps-vitest",
        "HEAD~2",
    ])?;
    // A GitHub remote that master tracks, for the pull request indicator.
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/trailmap-app/trailmap.git",
    ])?;
    git(&["config", "branch.master.remote", "origin"])?;
    git(&["config", "branch.master.merge", "refs/heads/master"])?;

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
        AnyWindowHandle, AppContext as _, Bounds, Entity, MouseButton, UpdateGlobal as _,
        VisualTestAppContext, WindowBounds, WindowHandle, WindowOptions, point, px, size,
    };
    use project::Project;
    use settings::SettingsStore;
    use util::ResultExt as _;
    use workspace::{AppState, MultiWorkspace, Workspace};

    pub fn run(project_path: PathBuf) -> Result<()> {
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&project_path)
            .args(["rev-parse", "HEAD"])
            .output()?;
        FIXTURE_HEAD
            .set(String::from_utf8_lossy(&head.stdout).trim().to_string())
            .ok();
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
            git::GitHostingProviderRegistry::default_global(cx).register_hosting_provider(
                Arc::new(git_hosting_providers::Github::public_instance()),
            );
            git_ui::pull_requests::set_github_client(
                evb_github::GitHubClient::new(
                    fake_github(),
                    Arc::new(FixedToken),
                    cx.background_executor().clone(),
                ),
                cx,
            );
            tasks_ui::init(cx);
            evb_run::init(cx);
            project::debugger::breakpoint_store::BreakpointStore::init(
                &app_state.client.clone().into(),
            );
            project::debugger::dap_store::DapStore::init(&app_state.client.clone().into(), cx);
            // Same layering as `zed::load_default_keymap` with the JetBrains base keymap.
            for (path, source) in [
                (
                    settings::DEFAULT_KEYMAP_PATH,
                    settings::KeybindSource::Default,
                ),
                (
                    "keymaps/macos/jetbrains.json",
                    settings::KeybindSource::Base,
                ),
                (settings::EVB_KEYMAP_PATH, settings::KeybindSource::Base),
                (
                    settings::SPECIFIC_OVERRIDES_KEYMAP_PATH,
                    settings::KeybindSource::Default,
                ),
            ] {
                if let Some(bindings) =
                    settings::KeymapFile::load_asset(path, Some(source), cx).log_err()
                {
                    cx.bind_keys(bindings);
                }
            }
        });
        cx.update(|cx| {
            let extension_root =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/evb-jetbrains-icons");
            let icon_theme = extension_root.join("icon_themes/jetbrains-icons.json");
            if let Some(bytes) = std::fs::read(&icon_theme).log_err() {
                theme::ThemeRegistry::global(cx)
                    .load_icon_theme(&icon_theme, &extension_root, bytes)
                    .log_err();
                theme_settings::reload_icon_theme(cx);
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
        workspace
            .update_in_window(any_window, &mut cx, |workspace, window, cx| {
                let indicator = cx.new(|cx| git_ui::PullRequestIndicator::new(workspace, cx));
                workspace.status_bar().update(cx, |status_bar, cx| {
                    status_bar.add_left_item(indicator, window, cx)
                });
            })
            .log_err();
        open_file(&workspace, any_window, "server/api/trip.get.ts", &mut cx)?;
        settle(&mut cx, 20);
        if let Ok(film_dir) = std::env::var("EVB_FILM_DIR") {
            return film(&workspace, any_window, &mut cx, Path::new(&film_dir));
        }
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
                workspace.focus_panel::<terminal_view::terminal_panel::TerminalPanel>(window, cx);
            })
            .log_err();
        settle(&mut cx, 3);
        cx.simulate_keystrokes(any_window, "cmd-j");
        settle(&mut cx, 3);
        let open_after_terminal = bottom_dock_open(&workspace, &mut cx);
        save(&mut cx, any_window, &output_dir, "04b-cmd-j-from-terminal")?;
        cx.simulate_keystrokes(any_window, "cmd-j");
        settle(&mut cx, 3);
        let open_after_editor = bottom_dock_open(&workspace, &mut cx);
        println!(
            "cmd-j: bottom dock open after pressing it in the terminal: {open_after_terminal}, \
             then in the editor: {open_after_editor}"
        );

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
        // Points in the Git Log panel of the fixed 1440x900 window: the third
        // commit, the `feature/trip-export` and `fix/map-tiles` branch rows, and the
        // User filter.
        right_click(&mut cx, any_window, point(px(975.), px(643.)));
        save(&mut cx, any_window, &output_dir, "06b-git-log-menu")?;
        cx.simulate_keystrokes(any_window, "escape");
        settle(&mut cx, 3);
        right_click(&mut cx, any_window, point(px(431.), px(606.)));
        save(&mut cx, any_window, &output_dir, "06c-branch-menu")?;
        cx.simulate_keystrokes(any_window, "escape");
        settle(&mut cx, 3);
        cx.simulate_click(any_window, point(px(720.), px(534.)), Default::default());
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "06d-user-filter")?;
        cx.simulate_keystrokes(any_window, "escape");
        settle(&mut cx, 3);
        cx.simulate_click(any_window, point(px(435.), px(645.)), Default::default());
        wait_real_time(&mut cx, Duration::from_secs(2));
        save(&mut cx, any_window, &output_dir, "06e-branch-selected")?;
        cx.simulate_click(any_window, point(px(435.), px(645.)), Default::default());
        wait_real_time(&mut cx, Duration::from_secs(2));

        set_theme(&mut cx, "IntelliJ Dark");
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "07-git-log-dark")?;

        open_file(&workspace, any_window, "app/components/AppBar.vue", &mut cx)?;
        wait_real_time(&mut cx, Duration::from_secs(2));
        save(&mut cx, any_window, &output_dir, "08-vue-dark")?;

        set_theme(&mut cx, "IntelliJ Light");
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "09-vue-light")?;

        open_file(&workspace, any_window, "server/api/trip.get.ts", &mut cx)?;
        cx.dispatch_action(any_window, git::Blame);
        wait_real_time(&mut cx, Duration::from_secs(3));
        save(&mut cx, any_window, &output_dir, "10-blame")?;

        // Points in the fixed 1440x900 window: a blame entry, the first item of its
        // context menu, and a line number.
        right_click(&mut cx, any_window, point(px(400.), px(200.)));
        save(&mut cx, any_window, &output_dir, "11-blame-menu")?;
        cx.simulate_click(any_window, point(px(487.), px(213.)), Default::default());
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "12-blame-closed")?;
        right_click(&mut cx, any_window, point(px(402.), px(200.)));
        save(&mut cx, any_window, &output_dir, "13-gutter-menu")?;
        // An empty part of the editor, to close the menu.
        cx.simulate_click(any_window, point(px(1000.), px(420.)), Default::default());
        settle(&mut cx, 3);

        save(&mut cx, any_window, &output_dir, "14-pull-request-status")?;
        // The indicator in the status bar of the fixed 1440x900 window.
        cx.simulate_click(
            any_window,
            point(px(PULL_REQUEST_X), px(889.)),
            Default::default(),
        );
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "15-pull-request-menu")?;
        cx.simulate_click(any_window, point(px(1000.), px(420.)), Default::default());
        settle(&mut cx, 3);
        // The review thread under line 10 of the open file.
        workspace
            .update_in_window(any_window, &mut cx, |workspace, window, cx| {
                workspace.close_panel::<git_ui::git_log_panel::GitLogPanel>(window, cx);
            })
            .log_err();
        cx.dispatch_action(any_window, git::Blame);
        settle(&mut cx, 5);
        save(&mut cx, any_window, &output_dir, "17-review-threads")?;
        right_click(&mut cx, any_window, point(px(525.), px(106.)));
        save(&mut cx, any_window, &output_dir, "18-review-gutter-menu")?;
        cx.simulate_click(any_window, point(px(1000.), px(600.)), Default::default());
        settle(&mut cx, 3);
        cx.simulate_click(
            any_window,
            point(px(PULL_REQUEST_X), px(889.)),
            Default::default(),
        );
        settle(&mut cx, 5);
        save(
            &mut cx,
            any_window,
            &output_dir,
            "19-pull-request-menu-threads",
        )?;
        cx.simulate_click(any_window, point(px(1000.), px(600.)), Default::default());
        settle(&mut cx, 3);
        PULL_REQUEST_OPEN.store(false, std::sync::atomic::Ordering::SeqCst);
        cx.dispatch_action(any_window, zed_actions::git::CreatePullRequest);
        wait_real_time(&mut cx, Duration::from_secs(2));
        save(&mut cx, any_window, &output_dir, "16-create-pull-request")?;

        println!("Captures written to {}", output_dir.display());
        Ok(())
    }

    const PULL_REQUEST_X: f32 = 80.;
    static FIXTURE_HEAD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static PULL_REQUEST_OPEN: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(true);

    struct FixedToken;

    impl evb_github::CredentialSource for FixedToken {
        fn token(
            &self,
            _host: &str,
        ) -> futures::future::BoxFuture<'static, Result<String, evb_github::GitHubError>> {
            Box::pin(futures::future::ready(Ok("fixture".to_string())))
        }

        fn invalidate(&self, _host: &str) {}
    }

    /// GitHub for the fixture: master has pull request #42 with a check still running, until
    /// `PULL_REQUEST_OPEN` is cleared.
    fn fake_github() -> Arc<dyn http_client::HttpClient> {
        use futures::AsyncReadExt as _;
        use serde_json::json;
        http_client::FakeHttpClient::create(|mut request| async move {
            let respond = |body: serde_json::Value| {
                http_client::Response::builder()
                    .status(200)
                    .body(http_client::AsyncBody::from(body.to_string()))
            };
            // The review's head is the fixture's HEAD, whose files the editor maps threads onto.
            let head = FIXTURE_HEAD.get().cloned().unwrap_or_default();
            let uri = request.uri().to_string();
            if uri.ends_with("/repos/trailmap-app/trailmap") {
                return Ok(respond(json!({ "default_branch": "main" }))?);
            }
            if uri.contains("/pulls?") {
                let pulls = if PULL_REQUEST_OPEN.load(std::sync::atomic::Ordering::SeqCst) {
                    json!([{
                        "number": 42,
                        "title": "Return 404 for trips the viewer cannot see",
                        "html_url": "https://github.com/trailmap-app/trailmap/pull/42",
                        "state": "open",
                        "draft": false,
                        "merged_at": null,
                        "base": { "ref": "main" },
                        "head": { "sha": head, "repo": { "full_name": "trailmap-app/trailmap" } }
                    }])
                } else {
                    json!([])
                };
                return Ok(respond(pulls)?);
            }
            let mut body = String::new();
            request.body_mut().read_to_string(&mut body).await?;
            let body: serde_json::Value = serde_json::from_str(&body)?;
            let query = body["query"].as_str().unwrap_or_default();
            let data = if query.contains("ReviewThreads") {
                let comment = |id: &str, author: &str, body: &str| {
                    json!({
                        "id": id, "author": { "login": author }, "body": body,
                        "createdAt": "2026-10-31T07:15:00Z",
                        "url": "https://github.com/trailmap-app/trailmap/pull/42#discussion_r1"
                    })
                };
                json!({ "repository": { "pullRequest": {
                    "id": "PR_42", "headRefOid": head,
                    "reviewThreads": { "pageInfo": null, "nodes": [
                        {
                            "id": "T1", "isResolved": false, "isOutdated": false,
                            "path": "server/api/trip.get.ts", "line": 10,
                            "viewerCanReply": true, "viewerCanResolve": true, "viewerCanUnresolve": false,
                            "comments": { "totalCount": 2, "nodes": [
                                comment("C1", "rivera", "Should a trip shared with someone else be a 404 too? The client cannot tell it from a deleted one."),
                                comment("C2", "alexchen", "Yes, on purpose: a 403 would confirm that the trip exists.")
                            ] }
                        },
                        {
                            "id": "T2", "isResolved": true, "isOutdated": false,
                            "path": "server/api/trip.get.ts", "line": 16,
                            "viewerCanReply": true, "viewerCanResolve": false, "viewerCanUnresolve": true,
                            "comments": { "totalCount": 1, "nodes": [
                                comment("C3", "rivera", "nosniff belongs with the other security headers.")
                            ] }
                        }
                    ] }
                } } })
            } else if query.contains("PullRequestStatus") {
                json!({ "repository": { "pullRequest": {
                    "mergeable": "MERGEABLE",
                    "reviewDecision": "APPROVED",
                    "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                        { "name": "typecheck", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://github.com/trailmap-app/trailmap/actions/runs/1", "isRequired": true },
                        { "name": "unit tests", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://github.com/trailmap-app/trailmap/actions/runs/2", "isRequired": true },
                        { "name": "e2e", "status": "IN_PROGRESS", "conclusion": null, "detailsUrl": "https://github.com/trailmap-app/trailmap/actions/runs/3" },
                        { "context": "deploy/preview", "state": "SUCCESS", "targetUrl": "https://preview.example.com" }
                    ] } } } }] }
                } } })
            } else {
                anyhow::bail!("unexpected GitHub request: {query}")
            };
            Ok(respond(json!({ "data": data }))?)
        })
    }

    /// A step of the landing page film: an app state, how long it shows, and the pointer.
    #[derive(serde::Serialize)]
    struct FilmStep {
        image: &'static str,
        frames: u32,
        cursor: Option<[f32; 2]>,
        /// The pointer clicks at the end of the step; the next step shows the result.
        click: bool,
        /// The part of the window the camera frames, 16:10 like the window.
        focus: Option<[f32; 4]>,
        /// Keys shown as pressed during the step.
        keys: Option<&'static str>,
    }

    /// Points in the fixed 1440x900 window.
    const RUN_BUTTON: [f32; 2] = [1370., 16.];
    const CONFIGURATION_PICKER: [f32; 2] = [1335., 16.];
    const COMMIT_ROW: [f32; 2] = [975., 643.];
    const PULL_REQUEST_BUTTON: [f32; 2] = [PULL_REQUEST_X, 889.];

    /// Records the states of the landing page film in both IntelliJ themes, with a manifest the
    /// landing's Remotion composition turns into video (landing/recorder/render.mjs).
    fn film(
        workspace: &Entity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut VisualTestAppContext,
        directory: &Path,
    ) -> Result<()> {
        let first_file = "app/components/AppBar.vue";
        let reviewed_file = "server/api/trip.get.ts";
        // The empty middle of the title bar: clicking it closes menus and selects nothing.
        let away = point(px(700.), px(16.));
        let dismiss = |cx: &mut VisualTestAppContext| {
            cx.simulate_keystrokes(window, "escape");
            cx.simulate_click(window, away, Default::default());
            settle(cx, 3);
        };
        for (theme, name) in [("IntelliJ Light", "light"), ("IntelliJ Dark", "dark")] {
            let output = directory.join(name);
            std::fs::create_dir_all(&output)?;
            set_theme(cx, theme);
            open_file(workspace, window, first_file, cx)?;
            settle(cx, 10);
            save(cx, window, &output, "workspace")?;

            cx.dispatch_action(window, evb_run::ChooseConfiguration);
            settle(cx, 5);
            save(cx, window, &output, "configurations")?;
            cx.dispatch_action(window, evb_run::ChooseConfiguration);
            settle(cx, 3);

            cx.dispatch_action(window, evb_run::Run);
            wait_real_time(cx, Duration::from_secs(4));
            save(cx, window, &output, "running")?;
            cx.dispatch_action(window, evb_run::Stop);
            wait_real_time(cx, Duration::from_secs(2));
            workspace
                .update_in_window(window, cx, |workspace, window, cx| {
                    workspace
                        .close_panel::<terminal_view::terminal_panel::TerminalPanel>(window, cx);
                })
                .log_err();

            cx.dispatch_action(window, git_ui::git_graph::Open);
            wait_real_time(cx, Duration::from_secs(3));
            // Both themes show the commit selected, as the right click selects it anyway.
            cx.simulate_click(
                window,
                point(px(COMMIT_ROW[0]), px(COMMIT_ROW[1])),
                Default::default(),
            );
            wait_real_time(cx, Duration::from_secs(1));
            save(cx, window, &output, "git-log")?;
            right_click(cx, window, point(px(COMMIT_ROW[0]), px(COMMIT_ROW[1])));
            save(cx, window, &output, "commit-menu")?;
            dismiss(cx);
            workspace
                .update_in_window(window, cx, |workspace, window, cx| {
                    workspace.close_panel::<git_ui::git_log_panel::GitLogPanel>(window, cx);
                })
                .log_err();

            open_file(workspace, window, reviewed_file, cx)?;
            settle(cx, 5);
            save(cx, window, &output, "review")?;
            cx.simulate_click(
                window,
                point(px(PULL_REQUEST_BUTTON[0]), px(PULL_REQUEST_BUTTON[1])),
                Default::default(),
            );
            settle(cx, 5);
            save(cx, window, &output, "pull-request")?;
            dismiss(cx);

            // Blame acts on the focused editor, which the dismissing click left.
            open_file(workspace, window, reviewed_file, cx)?;
            cx.dispatch_action(window, git::Blame);
            wait_real_time(cx, Duration::from_secs(3));
            save(cx, window, &output, "blame")?;
            cx.dispatch_action(window, git::Blame);
            settle(cx, 3);
        }

        let step = |image, frames, cursor, click, focus, keys| FilmStep {
            image,
            frames,
            cursor,
            click,
            focus,
            keys,
        };
        let steps = vec![
            step("workspace", 70, Some([760., 430.]), false, None, None),
            step(
                "workspace",
                36,
                Some(CONFIGURATION_PICKER),
                true,
                None,
                None,
            ),
            step(
                "configurations",
                60,
                Some(RUN_BUTTON),
                true,
                Some([940., 0., 480., 300.]),
                None,
            ),
            step("running", 85, Some(RUN_BUTTON), false, None, None),
            step("git-log", 70, Some(COMMIT_ROW), true, None, Some("⌘9")),
            step(
                "commit-menu",
                85,
                Some(COMMIT_ROW),
                false,
                Some([450., 110., 880., 550.]),
                None,
            ),
            step(
                "review",
                100,
                Some([900., 330.]),
                false,
                Some([240., 140., 880., 550.]),
                None,
            ),
            step("review", 30, Some(PULL_REQUEST_BUTTON), true, None, None),
            step(
                "pull-request",
                90,
                Some(PULL_REQUEST_BUTTON),
                false,
                Some([0., 400., 800., 500.]),
                None,
            ),
            step("blame", 80, Some([560., 330.]), false, None, Some("⌥⌘G B")),
        ];
        let manifest = serde_json::json!({
            "width": 1440,
            "height": 900,
            "fps": 30,
            "steps": steps,
        });
        std::fs::write(
            directory.join("manifest.json"),
            serde_json::to_string_pretty(&manifest)?,
        )?;
        println!("Film states written to {}", directory.display());
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

    fn bottom_dock_open(workspace: &Entity<Workspace>, cx: &mut VisualTestAppContext) -> bool {
        cx.update(|cx| {
            workspace
                .read(cx)
                .dock_at_position(workspace::dock::DockPosition::Bottom)
                .read(cx)
                .is_open()
        })
    }

    fn right_click(
        cx: &mut VisualTestAppContext,
        window: AnyWindowHandle,
        position: gpui::Point<gpui::Pixels>,
    ) {
        cx.simulate_mouse_down(window, position, MouseButton::Right, Default::default());
        cx.simulate_mouse_up(window, position, MouseButton::Right, Default::default());
        settle(cx, 5);
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
        workspace.update_in_window(window, cx, |workspace, window, cx| {
            let git_log_panel = git_ui::git_log_panel::GitLogPanel::new(workspace, window, cx);
            workspace.add_panel(git_log_panel, window, cx);
        })?;
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

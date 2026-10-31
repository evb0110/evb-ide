# EVB IDE

EVB IDE is an independent open-source fork of [Zed](https://github.com/zed-industries/zed)
that behaves more like WebStorm: IntelliJ themes and keymap, a run
configuration widget in the title bar, Commit and Push, and more reliable
Vue/TypeScript/PHP language support. It keeps its own settings and data, so it
runs next to an installed Zed without sharing state.

## What differs from Zed

| Area | Change | Where |
| --- | --- | --- |
| Identity | App "EVB IDE", binary `evb-ide`, bundle id `dev.evb.EVB-IDE`, data in `~/.config/evb-ide` and `~/Library/Application Support/EVB-IDE`, CLI link `/usr/local/bin/evb`, no zed.dev auto-update | `crates/paths`, `crates/release_channel`, `crates/zed/Cargo.toml`, `crates/cli`, `crates/install_cli`, `.cargo/config.toml` |
| Defaults | JetBrains keymap, IntelliJ Light/Dark themes, JetBrains Mono 13 without ligatures, Inter 13 for the UI, Project and Commit panels on the left, telemetry, the agent panel, edit predictions and sign-in off, Vue/PHP/Blade/Stylelint extensions installed automatically, Intelephense for PHP and Blade | `assets/settings/evb.json`, merged over `assets/settings/default.json` at startup |
| Key bindings | macOS: Run ⌃R, Debug ⌃D, Stop ⌘F2, choose configuration ⌃⌥R, Commit and Push ⌥⌘K, Git Log ⌘9, toggle the bottom dock ⌘J (JetBrains' Hover moves to F1). Linux and Windows, following WebStorm there: Run Shift+F10, Debug Shift+F9, Stop Ctrl+F2, choose configuration Alt+Shift+F10, Commit and Push Ctrl+Alt+K, Git Log Alt+9, bottom dock Ctrl+J | `assets/keymaps/evb.json` (macOS) and `assets/keymaps/linux/evb.json`, loaded after the base keymap |
| Run configurations | Title bar widget: configurations from `package.json` scripts (npm, pnpm, yarn or bun, from the lockfile), `composer.json` scripts and `.zed/tasks.json`; Run/Rerun, Debug (Node), Stop | `crates/evb_run` |
| Terminal | A Hide button in the terminal panel header, always visible, like WebStorm's tool windows | `crates/terminal_view/src/terminal_panel.rs` |
| Git Log tool window | The repository log lives in a bottom-dock panel (⌘9, like WebStorm's Git tool window) with the editor above it. A branches tree on its left (HEAD, Local, Remote per remote, folders at `/`) filters the log on click and has a branch menu on right-click. Filters: Branch, User ("me" and recent authors), Date (last 24 hours to last year, or any date) and Paths (the active file, its folder, or typed paths); User, Date and Paths are local-only. File histories still open as editor tabs | `crates/git_ui/src/git_log_panel.rs`, `crates/git_ui/src/git_graph.rs`, `LogSource::Filtered` in `crates/git/src/repository.rs` |
| Git commit menu | WebStorm's commit context menu in the Git Log and History: Copy Revision Number, Create Patch, Cherry-Pick, Checkout Revision, Reset Current Branch to Here (soft/mixed/hard/keep), Revert, Undo Commit, Edit Commit Message, Fixup with Staged Changes, Drop Commit, Push All up to Here, a submenu per branch at the commit (checkout, merge, rebase, delete), New Branch, New Tag, Go to Parent/Child Commit, View on GitHub. History rewrites need a linear history after the commit, ask before rewriting pushed commits and abort a failed rebase; they run git directly, so they work in local projects only | `crates/git_ui/src/commit_operations.rs`, `crates/git_ui/src/commit_context_menu.rs`, `Repository::run_git_command` in `crates/project/src/git_store.rs` |
| GitHub pull requests | The status bar shows the current branch's pull request: number, draft/merged/closed state, a checks mark (failing checks win, then required ones gate passing), conflicts; its menu opens it, copies the link and lists the checks. It refreshes on branch, commit and push changes and on a timer (30 s while checks run, 2 min when open, 5 min otherwise, backing off after failures and rate limits). Create Pull Request (also "Commit, Push and Create Pull Request…" in the commit menu, offered for GitHub branches without an open pull request) opens a form filled like `gh pr create --fill`. The base repository is chosen like gh (`gh repo set-default`, by remote or by name, then upstream, github, origin); the head is where `git push` sends the branch (`pushRemote`, `remote.pushDefault`, the upstream), looked up by its exact `owner:branch`. The form opens only for the branch that was pushed. Credentials come from `GH_TOKEN`/`GITHUB_TOKEN` (enterprise tokens only for `GH_HOST`) or `gh auth token`, run without token variables. Other hosts keep the browser link. Hide it with `status_bar.pull_request_button` | `crates/evb_github` (client ported from T3 Code), `crates/git_ui/src/pull_requests.rs` |
| Pull request review | The open pull request's review threads show under their lines in the editor (resolved ones collapsed, with Show/Hide), with Reply, Resolve/Unresolve and Open on GitHub. Lines are mapped between the buffer and the file at the pull request's head commit, so threads follow local edits and threads on locally changed or deleted lines stay in the list only. "Add Pull Request Comment…" in the line-number menu (or `git: add pull request comment`) comments on the head commit's line and refuses locally changed lines; GitHub accepts lines inside the diff only. Writes check that the review is still the current branch's. The status bar menu lists the threads (outdated ones open on GitHub) and has Submit Review… (Comment, Request Changes, Approve). Threads refresh with the status and after each write | `crates/git_ui/src/pull_request_review.rs`, review calls in `crates/evb_github` |
| Git | Commit and Push in the commit split button; a Branch filter in the git log; a compact blame column (short date and last name, like WebStorm's annotations) with an "Annotate with Git Blame" toggle in the right-click menus of both the blame column and the line numbers | `crates/git_ui/src/git_panel.rs`, `crates/git_ui/src/git_graph.rs`, `crates/git_ui/src/blame_ui.rs`, `crates/editor/src/git/blame.rs` |
| Language servers | Completion trigger context per server (fixes Vue completions only appearing on manual invoke), rename tries the next capable server, Vue↔TypeScript bridge scoped to the worktree and waiting for vtsls to start | `crates/project/src/lsp_command.rs`, `crates/project/src/project.rs`, `crates/project/src/lsp_store/vue_language_server_ext.rs` |
| Themes | IntelliJ Light and IntelliJ Dark from the Apache-2.0 intellij-community sources | `assets/themes/intellij`, generated by `script/evb-generate-intellij-themes` |
| File icons | JetBrains Icons Light/Dark, with the Apache-2.0 expUI SVGs embedded into the application for fresh installations | `extensions/evb-jetbrains-icons`, `crates/assets` |
| Fonts | JetBrains Mono and Inter (OFL) | `assets/fonts/jetbrains-mono`, `assets/fonts/inter` |

Everything else is upstream Zed. Settings you put in `~/.config/evb-ide/settings.json`
override the EVB defaults, the same way they override Zed's.

## Build and install

Requirements: Xcode with the Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`),
`cmake`, and rustup (the toolchain in `rust-toolchain.toml` installs itself).

```sh
script/evb-install          # release build, assemble "EVB IDE.app", install into /Applications,
                            # with bundled icons and dependency license notices
script/evb-install -n       # build and assemble only (target/evb-bundle/EVB IDE.app)
cargo run                   # debug build of the app
```

The release build takes a while and needs about 20 GB of memory at link time.

`script/evb-install` is the only supported packaging path. Upstream's
`script/bundle-mac`, `bundle-linux` and `bundle-windows.ps1` still expect a
binary named `zed` and Zed's signing setup, and are not used by EVB IDE. The
app registers no URL scheme, since it only understands `zed://` links, which
belong to an installed Zed. It also uses its own single-instance port, so it
runs at the same time as Zed.

## Keep up with Zed

```sh
script/evb-sync-upstream    # fetch and merge zed-industries/zed main, preserving published history
```

After a sync, run the checks below and reinstall.

## Public releases

The fork's version is in `EVB_VERSION`; the upstream crate versions remain
available for dependency and extension compatibility. Public downloads are at
https://github.com/evb0110/evb-ide/releases and the landing page is at
https://evb-ide.vercel.app. Releases ship macOS (Apple Silicon), Linux x64 and Windows x64.

Releases build on GitHub Actions (`.github/workflows/evb-release.yml`), so they
need no local release build. Bump `EVB_VERSION`, write the user-facing notes
in `release-notes/vVERSION.md`, push to `origin/main` with a green EVB CI run,
then push the tag `vVERSION`. The workflow signs and notarizes the macOS app
with the repository secrets (`CSC_LINK`, `CSC_KEY_PASSWORD`, `APPLE_API_KEY`,
`APPLE_API_KEY_ID`, `APPLE_API_ISSUER`; see `script/evb-ci-macos-signing`),
builds the Linux archive with `script/evb-bundle-linux` on Ubuntu 22.04 and the
portable Windows archive with `script/evb-bundle-windows.ps1`, and publishes
all three with the source archive and checksums. A manual run of the
workflow builds and uploads the artifacts without publishing.

To release from a Mac instead, set `EVB_SIGNING_IDENTITY` to your Developer ID
Application identity and, for notarization, `EVB_NOTARY_PROFILE` or
`APPLE_API_KEY` (a local .p8 path), `APPLE_API_KEY_ID` and `APPLE_API_ISSUER`.
Never commit credentials.

```sh
script/evb-release             # build, sign, notarize, package, vendor sources, checksum
# Verify the packaged app starts with an isolated fresh profile.
script/evb-release --publish   # upload and publish that exact prepared revision
```

The release script does not install over your application. It requires a clean
checkout and creates the DMG, corresponding source archive, checksums, and
release notes under `.devkit/releases/vVERSION`. The source archive includes
vendored Rust dependencies and can be built without a Git checkout. System
toolchains and native components fetched by third-party build scripts retain
their documented prerequisites. Public release assets must stay available
together with their corresponding source.

The landing page has its own locked dependencies and guide in
[landing/README.md](landing/README.md). Run `script/evb-deploy-landing` for its
Vercel production deployment. Upstream Zed workflows rely on Zed-owned runners;
the fork uses `EVB CI` for its own publication checks.

## Checks

```sh
cargo test -p evb_run
cargo test -p git_ui --lib commit_operations
cargo test -p git_ui --lib git_graph
cargo test -p git --lib initial_graph_data   # run with TMPDIR outside this checkout
cargo test -p settings --lib evb_default_settings_tests
cargo test -p project --lib completion_context_tests
cargo test -p project --features test-support --test integration -- rename vue completion
cargo run -p zed --bin evb-visual-capture --features visual-tests
```

`evb-visual-capture` opens EVB IDE in hidden, unfocused windows on a throwaway
git project and writes screenshots of the workspace, the run configuration
menu, a running and a stopped `npm run dev`, the Commit panel, the git log,
both themes, the blame column with its menus, and the pull request indicator, its menu, review threads in the editor, the Create Pull Request form (against a fake GitHub) and two projects as window tabs to `target/evb_captures` (or `EVB_CAPTURE_DIR`).

## Updating the themes

Edit the palette in `script/evb-generate-intellij-themes` and run it. The
colors come from intellij-community's New UI `Light` (light header) and `Dark`
themes and their editor schemes; keep the attribution in
`assets/themes/intellij/LICENSE` when changing them.

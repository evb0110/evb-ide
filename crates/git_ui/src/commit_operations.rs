use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::Editor;
use git::{Oid, repository::Branch};
use gpui::{
    App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, WeakEntity, Window,
};
use menu::{Cancel, Confirm};
use notifications::status_toast::StatusToast;
use project::git_store::Repository;
use ui::{ContextMenu, KeyBinding, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace, notifications::DetachAndPromptErr as _};

/// Turns the first line of an interactive rebase todo list into `reword`, after
/// checking that it picks the commit in `EVB_REWORD_SHA`; any other todo list
/// fails the rebase instead of rewording the wrong commit.
const REWORD_SEQUENCE_EDITOR: &str = r#"sh -c 'read -r command sha rest < "$1" && [ "$command" = pick ] && [ -n "$sha" ] && case "$EVB_REWORD_SHA" in "$sha"*) ;; *) exit 1 ;; esac && sed "1s/^pick /reword /" "$1" > "$1.evb" && mv "$1.evb" "$1"' evb-sequence-editor"#;
/// Writes `EVB_COMMIT_MESSAGE` into the file git asks to edit.
const MESSAGE_EDITOR: &str =
    r#"sh -c 'printf "%s\n" "$EVB_COMMIT_MESSAGE" > "$1"' evb-commit-message-editor"#;

/// A commit that a log or history context menu acts on.
#[derive(Clone)]
pub(crate) struct CommitTarget {
    pub(crate) sha: Oid,
    pub(crate) repository: WeakEntity<Repository>,
    pub(crate) workspace: WeakEntity<Workspace>,
}

impl CommitTarget {
    fn short_sha(&self) -> String {
        self.sha.display_short()
    }

    pub(crate) fn is_head(&self, cx: &App) -> bool {
        self.repository.upgrade().is_some_and(|repository| {
            repository
                .read(cx)
                .head_commit
                .as_ref()
                .is_some_and(|head| head.sha.as_ref() == self.sha.to_string())
        })
    }

    /// Branches whose tip is this commit, local ones first.
    pub(crate) fn branches_at_commit(&self, cx: &App) -> Vec<Branch> {
        let Some(repository) = self.repository.upgrade() else {
            return Vec::new();
        };
        let sha = self.sha.to_string();
        let mut branches = repository
            .read(cx)
            .branch_list
            .iter()
            .filter(|branch| {
                branch
                    .most_recent_commit
                    .as_ref()
                    .is_some_and(|commit| commit.sha.as_ref() == sha)
            })
            .cloned()
            .collect::<Vec<_>>();
        branches.sort_by(|left, right| {
            (left.is_remote(), left.name()).cmp(&(right.is_remote(), right.name()))
        });
        branches
    }

    /// Runs one git command and reports success in a toast, failure in a prompt.
    fn run(
        &self,
        status: String,
        args: Vec<String>,
        success_message: String,
        error_title: &'static str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let repository = repository.upgrade().context("the repository was closed")?;
                git(&repository, &status, args, HashMap::default(), cx).await?;
                show_toast(&workspace, success_message, cx);
                Ok(())
            })
            .detach_and_prompt_err(error_title, window, cx, |error, _, _| {
                Some(format!("{error:#}"))
            });
    }
}

async fn git(
    repository: &Entity<Repository>,
    status: &str,
    args: Vec<String>,
    env: HashMap<String, String>,
    cx: &mut AsyncWindowContext,
) -> Result<String> {
    let status = SharedString::from(status.to_string());
    repository
        .update(cx, |repository, cx| {
            repository.run_git_command(Some(status), args, env, cx)
        })
        .await?
}

/// Runs a git command that changes the repository only if HEAD is still `head`, checked in the
/// same git job so no other queued operation can move HEAD in between.
async fn git_at_head(
    repository: &Entity<Repository>,
    status: &str,
    args: Vec<String>,
    env: HashMap<String, String>,
    head: &HeadState,
    cx: &mut AsyncWindowContext,
) -> Result<String> {
    let status = SharedString::from(status.to_string());
    let (sha, branch) = (head.sha.clone(), head.branch.clone());
    repository
        .update(cx, |repository, cx| {
            repository.run_git_command_at_head(Some(status), args, env, sha, branch, cx)
        })
        .await?
}

async fn parent_count(
    repository: &Entity<Repository>,
    sha: Oid,
    cx: &mut AsyncWindowContext,
) -> Result<usize> {
    let parents = git(
        repository,
        "Reading commit",
        args(["rev-list", "--parents", "-n", "1", &sha.to_string()]),
        HashMap::default(),
        cx,
    )
    .await?;
    Ok(parents.split_whitespace().count().saturating_sub(1))
}

pub(crate) fn args<const N: usize>(args: [&str; N]) -> Vec<String> {
    args.into_iter().map(str::to_string).collect()
}

fn show_toast(workspace: &WeakEntity<Workspace>, message: String, cx: &mut AsyncWindowContext) {
    workspace
        .update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _| this);
            workspace.toggle_status_toast(toast, cx);
        })
        .log_err();
}

async fn confirm(
    level: PromptLevel,
    message: String,
    detail: Option<String>,
    confirm_label: &str,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let answer = cx.update(|window, cx| {
        window.prompt(
            level,
            &message,
            detail.as_deref(),
            &[confirm_label, "Cancel"],
            cx,
        )
    })?;
    Ok(answer.await? == 0)
}

/// Where HEAD pointed when the user was asked to confirm an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadState {
    sha: String,
    /// The full ref, such as `refs/heads/main`: a short name is ambiguous when a tag shares it.
    branch: Option<String>,
}

impl HeadState {
    fn label(&self) -> String {
        match &self.branch {
            Some(branch) => format!("'{}'", branch.strip_prefix("refs/heads/").unwrap_or(branch)),
            None => "HEAD".to_string(),
        }
    }

    fn label_with_commit(&self) -> String {
        let short_sha = self.sha.get(..7).unwrap_or(&self.sha);
        format!("{} at {short_sha}", self.label())
    }
}

async fn read_head(
    repository: &Entity<Repository>,
    cx: &mut AsyncWindowContext,
) -> Result<HeadState> {
    let sha = git(
        repository,
        "Reading HEAD",
        args(["rev-parse", "--verify", "HEAD"]),
        HashMap::default(),
        cx,
    )
    .await?
    .trim()
    .to_string();
    let branch = git(
        repository,
        "Reading HEAD",
        args(["symbolic-ref", "-q", "HEAD"]),
        HashMap::default(),
        cx,
    )
    .await
    .ok()
    .map(|branch| branch.trim().to_string())
    .filter(|branch| !branch.is_empty());
    Ok(HeadState { sha, branch })
}

/// Fails when HEAD moved since `expected` was read, so an operation confirmed
/// for one branch never runs on another.
async fn ensure_head_unchanged(
    repository: &Entity<Repository>,
    expected: &HeadState,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let current = read_head(repository, cx).await?;
    anyhow::ensure!(
        current == *expected,
        "HEAD moved from {} to {} while the operation was waiting; nothing was changed.",
        expected.label_with_commit(),
        current.label_with_commit()
    );
    Ok(())
}

/// The rebase, merge, cherry-pick or revert that is stopped in the repository, if any.
async fn operation_in_progress(
    repository: &Entity<Repository>,
    cx: &mut AsyncWindowContext,
) -> Result<Option<&'static str>> {
    let work_directory = repository.update(cx, |repository, _| {
        repository.work_directory_abs_path.to_path_buf()
    });
    for (git_path, operation) in [
        ("rebase-merge", "A rebase"),
        ("rebase-apply", "A rebase"),
        ("MERGE_HEAD", "A merge"),
        ("CHERRY_PICK_HEAD", "A cherry-pick"),
        ("REVERT_HEAD", "A revert"),
    ] {
        let path = git(
            repository,
            "Checking repository state",
            args(["rev-parse", "--git-path", git_path]),
            HashMap::default(),
            cx,
        )
        .await?;
        let path = work_directory.join(path.trim());
        if cx.background_spawn(async move { path.exists() }).await {
            return Ok(Some(operation));
        }
    }
    Ok(None)
}

async fn ensure_no_operation_in_progress(
    repository: &Entity<Repository>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    if let Some(operation) = operation_in_progress(repository, cx).await? {
        anyhow::bail!("{operation} is in progress; finish or abort it first.");
    }
    Ok(())
}

/// Asks before rewriting a commit that a remote branch contains. Returns
/// whether to go ahead.
async fn confirm_rewriting_pushed_commit(
    target: &CommitTarget,
    repository: &Entity<Repository>,
    action: &str,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    confirm_rewriting_pushed(&target.sha.to_string(), repository, action, cx).await
}

/// Asks before rewriting `sha` when a remote branch contains it.
async fn confirm_rewriting_pushed(
    sha: &str,
    repository: &Entity<Repository>,
    action: &str,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let short_sha = sha.get(..7).unwrap_or(sha);
    let pushed_to = git(
        repository,
        "Checking history",
        args([
            "branch",
            "-r",
            "--contains",
            sha,
            "--format=%(refname:short)",
        ]),
        HashMap::default(),
        cx,
    )
    .await?;
    let pushed_to = pushed_to
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if pushed_to.is_empty() {
        return Ok(true);
    }
    confirm(
        PromptLevel::Warning,
        format!("{short_sha} is already pushed to {}", pushed_to.join(", ")),
        Some(format!(
            "{action} it means you will have to force push. Continue?"
        )),
        "Continue",
        cx,
    )
    .await
}

/// Checks that rewriting history from `target` up to `head`, the HEAD the
/// user started from, is possible with a linear rebase, and asks before
/// rewriting pushed commits. Returns the rebase base argument, or `None` when
/// the user cancelled.
async fn prepare_history_rewrite(
    target: &CommitTarget,
    repository: &Entity<Repository>,
    head: &HeadState,
    cx: &mut AsyncWindowContext,
) -> Result<Option<String>> {
    ensure_no_operation_in_progress(repository, cx).await?;
    ensure_head_unchanged(repository, head, cx).await?;
    let sha = target.sha.to_string();
    let short_sha = target.short_sha();
    let parent_count = parent_count(repository, target.sha, cx).await?;
    anyhow::ensure!(
        parent_count <= 1,
        "{short_sha} is a merge commit, which can't be rewritten here."
    );
    git(
        repository,
        "Checking history",
        args(["merge-base", "--is-ancestor", &sha, &head.sha]),
        HashMap::default(),
        cx,
    )
    .await
    .map_err(|_| anyhow::anyhow!("{short_sha} is not in the current branch."))?;
    let merges = git(
        repository,
        "Checking history",
        args(["rev-list", "--merges", &format!("{sha}..{}", head.sha)]),
        HashMap::default(),
        cx,
    )
    .await?;
    anyhow::ensure!(
        merges.trim().is_empty(),
        "The commits after {short_sha} include merge commits, which can't be rewritten here."
    );
    if !confirm_rewriting_pushed_commit(target, repository, "Rewriting", cx).await? {
        return Ok(None);
    }
    let base = if parent_count == 0 {
        "--root".to_string()
    } else {
        format!("{sha}^")
    };
    Ok(Some(base))
}

/// Whether the stopped rebase, if any, is the one started from `head`: git
/// records the HEAD and branch a rebase started from.
async fn stopped_rebase_started_from(
    repository: &Entity<Repository>,
    head: &HeadState,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let work_directory = repository.update(cx, |repository, _| {
        repository.work_directory_abs_path.to_path_buf()
    });
    let expected_head_name = head
        .branch
        .clone()
        .unwrap_or_else(|| "detached HEAD".to_string());
    for state_directory in ["rebase-merge", "rebase-apply"] {
        let path = git(
            repository,
            "Checking repository state",
            args(["rev-parse", "--git-path", state_directory]),
            HashMap::default(),
            cx,
        )
        .await?;
        let directory = work_directory.join(path.trim());
        let (orig_head, head_name) = cx
            .background_spawn(async move {
                (
                    std::fs::read_to_string(directory.join("orig-head")).ok(),
                    std::fs::read_to_string(directory.join("head-name")).ok(),
                )
            })
            .await;
        if let (Some(orig_head), Some(head_name)) = (orig_head, head_name) {
            return Ok(orig_head.trim() == head.sha && head_name.trim() == expected_head_name);
        }
    }
    Ok(false)
}

/// The commits a rewrite from `base` (a commit, or `--root`) replays up to `head_sha`.
fn replayed_range(base: &str, head_sha: &str) -> String {
    if base == "--root" {
        head_sha.to_string()
    } else {
        format!("{base}..{head_sha}")
    }
}

/// Refuses a rewrite that would lose local work `--autostash` does not protect: staged changes,
/// which its restore puts back as unstaged only, and ignored files at paths that the replayed
/// commits (`replayed`, a revision range) write or delete, which git overwrites silently.
async fn ensure_rewrite_keeps_local_work(
    repository: &Entity<Repository>,
    replayed: &str,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    git(
        repository,
        "Checking local changes",
        args(["diff", "--cached", "--quiet"]),
        HashMap::default(),
        cx,
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Commit or unstage your staged changes first: rewriting history would not keep them staged."
        )
    })?;
    let touched = git(
        repository,
        "Checking local changes",
        // NUL-separated: in its line output git quotes names with unusual characters.
        args([
            "log",
            "-z",
            "--format=",
            "--name-only",
            "--no-renames",
            replayed,
            "--",
        ]),
        HashMap::default(),
        cx,
    )
    .await?;
    let mut touched = touched
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    touched.sort_unstable();
    touched.dedup();
    let mut ignored = Vec::new();
    for paths in touched.chunks(500) {
        let mut command = args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
        ]);
        command.extend(paths.iter().map(|path| format!(":(literal){path}")));
        let output = git(
            repository,
            "Checking local changes",
            command,
            HashMap::default(),
            cx,
        )
        .await?;
        ignored.extend(
            output
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_string),
        );
    }
    if let Some(first) = ignored.first() {
        anyhow::bail!(
            "Rewriting history would replace ignored local files ({first}{}). Move them aside first.",
            if ignored.len() > 1 {
                format!(" and {} more", ignored.len() - 1)
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// Runs a rebase of `head`, the HEAD the user confirmed, and aborts it when it
/// fails, so a failed rewrite leaves the branch as it was. It refuses to start
/// while another operation is stopped, or when it would lose local work, and
/// only aborts a stopped rebase that started from `head`. `replayed` is the
/// revision range whose commits the rebase writes into the working tree.
async fn rebase(
    repository: &Entity<Repository>,
    status: &str,
    rebase_args: Vec<String>,
    env: HashMap<String, String>,
    head: &HeadState,
    replayed: &str,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    ensure_no_operation_in_progress(repository, cx).await?;
    ensure_head_unchanged(repository, head, cx).await?;
    ensure_rewrite_keeps_local_work(repository, replayed, cx).await?;
    let mut command = args([
        "-c",
        "commit.cleanup=whitespace",
        "-c",
        "rebase.abbreviateCommands=false",
        "rebase",
    ]);
    command.extend(rebase_args);
    let result = git_at_head(repository, status, command, env, head, cx).await;
    if let Err(error) = result {
        if !stopped_rebase_started_from(repository, head, cx).await? {
            return Err(error.context("The rebase failed"));
        }
        let abort = git(
            repository,
            "Aborting rebase",
            args(["rebase", "--abort"]),
            HashMap::default(),
            cx,
        )
        .await;
        return Err(match abort {
            Ok(_) => error.context("The rebase failed and was aborted"),
            Err(abort_error) => error.context(format!(
                "The rebase failed, and aborting it failed too, so the rebase is still in \
                 progress; finish or abort it with git. Abort error: {abort_error:#}"
            )),
        });
    }
    // Git reports success even when putting the stashed local changes back conflicts.
    let conflicted = git(
        repository,
        "Checking local changes",
        args(["diff", "--name-only", "--diff-filter=U"]),
        HashMap::default(),
        cx,
    )
    .await?;
    if let Some(first) = conflicted.lines().next() {
        anyhow::bail!(
            "History was rewritten, but your local changes conflicted when they were put back \
             (in {first}). Resolve the conflicts; your changes are also kept in `git stash list`."
        );
    }
    Ok(())
}

pub(crate) fn cherry_pick(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    apply_commit(
        target,
        "cherry-pick",
        "Cherry-picking",
        "Cherry-picked",
        "Cherry-pick failed",
        window,
        cx,
    );
}

pub(crate) fn revert(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    apply_commit(
        target,
        "revert",
        "Reverting",
        "Reverted",
        "Revert failed",
        window,
        cx,
    );
}

/// Cherry-picks or reverts a commit; merge commits are taken relative to their first parent.
fn apply_commit(
    target: &CommitTarget,
    subcommand: &'static str,
    status: &'static str,
    done: &'static str,
    error_title: &'static str,
    window: &mut Window,
    cx: &mut App,
) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            ensure_no_operation_in_progress(&repository, cx).await?;
            let mut command = args([subcommand]);
            if subcommand == "revert" {
                command.push("--no-edit".to_string());
            }
            if parent_count(&repository, target.sha, cx).await? > 1 {
                command.extend(args(["-m", "1"]));
            }
            command.push(target.sha.to_string());
            let result = git(
                &repository,
                &format!("{status} {}", target.short_sha()),
                command,
                HashMap::default(),
                cx,
            )
            .await;
            if let Err(error) = result {
                // Like WebStorm, conflicts stay for the user to resolve rather than being undone.
                if operation_in_progress(&repository, cx).await?.is_some() {
                    return Err(error.context(format!(
                        "{} stopped on conflicts. Resolve them and commit, or run \
                         `git {subcommand} --abort` to undo it",
                        target.short_sha()
                    )));
                }
                return Err(error);
            }
            show_toast(
                &target.workspace,
                format!("{done} {}", target.short_sha()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err(error_title, window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn checkout_revision(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    target.run(
        format!("Checking out {}", target.short_sha()),
        args(["switch", "--detach", &target.sha.to_string()]),
        format!("Checked out {}", target.short_sha()),
        "Checkout failed",
        window,
        cx,
    );
}

pub(crate) fn reset_current_branch(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let head = read_head(&repository, cx).await?;
            let branch = head.label();
            let modes = ["Soft", "Mixed", "Hard", "Keep", "Cancel"];
            let answer = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    &format!("Reset {branch} to {}?", target.short_sha()),
                    Some(
                        "Soft: files and the index are kept.\n\
                         Mixed: files are kept, the index is reset.\n\
                         Hard: files and the index are reset; local changes are lost.\n\
                         Keep: files are reset, local changes are kept.",
                    ),
                    &modes,
                    cx,
                )
            })?;
            let Some(mode) = modes.get(answer.await?).filter(|mode| **mode != "Cancel") else {
                return Ok(());
            };
            let flag = format!("--{}", mode.to_lowercase());
            git_at_head(
                &repository,
                &format!("Resetting to {}", target.short_sha()),
                args(["reset", &flag, &target.sha.to_string()]),
                HashMap::default(),
                &head,
                cx,
            )
            .await?;
            show_toast(
                &target.workspace,
                format!("Reset {branch} to {} ({mode})", target.short_sha()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Reset failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn undo_commit(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            ensure_no_operation_in_progress(&repository, cx).await?;
            let head = read_head(&repository, cx).await?;
            let sha = target.sha.to_string();
            anyhow::ensure!(
                head.sha == sha,
                "Only the latest commit of the current branch can be undone."
            );
            anyhow::ensure!(
                parent_count(&repository, target.sha, cx).await? == 1,
                "Only a commit with one parent can be undone."
            );
            if !confirm_rewriting_pushed_commit(&target, &repository, "Undoing", cx).await? {
                return Ok(());
            }
            git_at_head(
                &repository,
                "Undoing commit",
                args(["reset", "--soft", &format!("{sha}^")]),
                HashMap::default(),
                &head,
                cx,
            )
            .await?;
            show_toast(
                &target.workspace,
                format!("Undid {}; its changes are staged", target.short_sha()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Undo commit failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn drop_commit(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            anyhow::ensure!(
                parent_count(&repository, target.sha, cx).await? == 1,
                "Only a commit with one parent can be dropped."
            );
            let head = read_head(&repository, cx).await?;
            let Some(base) = prepare_history_rewrite(&target, &repository, &head, cx).await? else {
                return Ok(());
            };
            if !confirm(
                PromptLevel::Warning,
                format!("Drop {}?", target.short_sha()),
                Some("Its changes are removed from the branch history.".to_string()),
                "Drop",
                cx,
            )
            .await?
            {
                return Ok(());
            }
            let sha = target.sha.to_string();
            rebase(
                &repository,
                &format!("Dropping {}", target.short_sha()),
                args([
                    "--autostash",
                    "--no-rebase-merges",
                    "--onto",
                    &format!("{sha}^"),
                    &sha,
                ]),
                HashMap::default(),
                &head,
                &replayed_range(&base, &head.sha),
                cx,
            )
            .await?;
            show_toast(
                &target.workspace,
                format!("Dropped {}", target.short_sha()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Drop commit failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

/// Folds the staged changes into `target`, keeping its message.
pub(crate) fn fixup(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let has_staged_changes = git(
                &repository,
                "Checking staged changes",
                args(["diff", "--cached", "--quiet"]),
                HashMap::default(),
                cx,
            )
            .await
            .is_err();
            anyhow::ensure!(
                has_staged_changes,
                "Stage the changes to fold into {} first.",
                target.short_sha()
            );
            let head = read_head(&repository, cx).await?;
            let Some(base) = prepare_history_rewrite(&target, &repository, &head, cx).await? else {
                return Ok(());
            };
            let sha = target.sha.to_string();
            git_at_head(
                &repository,
                "Committing fixup",
                args(["commit", &format!("--fixup={sha}")]),
                HashMap::default(),
                &head,
                cx,
            )
            .await?;
            // The fold rebases the fixup commit on top of the confirmed HEAD.
            let after_fixup = read_head(&repository, cx).await?;
            let fixup_parent = git(
                &repository,
                "Checking history",
                args(["rev-parse", &format!("{}^", after_fixup.sha)]),
                HashMap::default(),
                cx,
            )
            .await?;
            anyhow::ensure!(
                after_fixup.branch == head.branch && fixup_parent.trim() == head.sha,
                "HEAD moved while the fixup commit was created; it was left unfolded."
            );
            rebase(
                &repository,
                &format!("Folding changes into {}", target.short_sha()),
                args([
                    "-i",
                    "--autosquash",
                    "--autostash",
                    "--no-rebase-merges",
                    &base,
                ]),
                HashMap::default(),
                &after_fixup,
                &replayed_range(&base, &after_fixup.sha),
                cx,
            )
            .await
            .context("The fixup commit was created, but folding it failed")?;
            show_toast(
                &target.workspace,
                format!("Folded the staged changes into {}", target.short_sha()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Fixup failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn push_up_to(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let sha = target.sha.to_string();
            let branch = git(
                &repository,
                "Reading branch",
                args(["symbolic-ref", "HEAD"]),
                HashMap::default(),
                cx,
            )
            .await
            .map_err(|_| anyhow::anyhow!("HEAD is detached; check out a branch first."))?;
            // `--short` would say `heads/main` when a tag is also named `main`.
            let branch = branch.trim();
            let branch = branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_string();
            let config = async |key: String, cx: &mut AsyncWindowContext| {
                git(
                    &repository,
                    "Reading push settings",
                    args(["config", "--get", &key]),
                    HashMap::default(),
                    cx,
                )
                .await
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            };
            // Git resolves the push remote (pushRemote, remote.pushDefault,
            // then the remote it pulls from) and, when a push refspec maps
            // the branch, its destination.
            let destination = git(
                &repository,
                "Reading push settings",
                args([
                    "for-each-ref",
                    "--format=%(push:remotename)%00%(push:remoteref)",
                    &format!("refs/heads/{branch}"),
                ]),
                HashMap::default(),
                cx,
            )
            .await?;
            let (remote, mapped_ref) = destination
                .trim_end()
                .split_once('\0')
                .unwrap_or((destination.trim_end(), ""));
            anyhow::ensure!(
                !remote.is_empty(),
                "'{branch}' has no remote to push to; push it once first."
            );
            let remote = remote.to_string();
            let push_refspecs = git(
                &repository,
                "Reading push settings",
                args(["config", "--get-all", &format!("remote.{remote}.push")]),
                HashMap::default(),
                cx,
            )
            .await
            .unwrap_or_default();
            let push_refspec_count = push_refspecs.lines().filter(|line| !line.trim().is_empty()).count();
            // Configured push refspecs decide alone; a branch they leave out, or send to several
            // places, is not pushed to an invented destination.
            anyhow::ensure!(
                push_refspec_count == 0 || (push_refspec_count == 1 && !mapped_ref.is_empty()),
                "The push settings of {remote} (remote.{remote}.push) do not send '{branch}' to one \
                 branch; push it from the terminal."
            );
            // Without a refspec, git's default pushes to the tracked branch on
            // the remote it pulls from and to the same name elsewhere.
            let merge_ref = if !mapped_ref.is_empty() {
                mapped_ref.to_string()
            } else {
                let upstream_remote = config(format!("branch.{branch}.remote"), cx).await;
                match config(format!("branch.{branch}.merge"), cx).await {
                    Some(merge_ref) if upstream_remote.as_ref() == Some(&remote) => merge_ref,
                    _ => format!("refs/heads/{branch}"),
                }
            };
            git(
                &repository,
                "Checking history",
                args(["merge-base", "--is-ancestor", &sha, "HEAD"]),
                HashMap::default(),
                cx,
            )
            .await
            .map_err(|_| anyhow::anyhow!("{} is not in '{branch}'.", target.short_sha()))?;
            git(
                &repository,
                &format!("Pushing up to {}", target.short_sha()),
                args(["push", &remote, &format!("{sha}:{merge_ref}")]),
                HashMap::default(),
                cx,
            )
            .await?;
            let remote_branch = merge_ref.strip_prefix("refs/heads/").unwrap_or(&merge_ref);
            show_toast(
                &target.workspace,
                format!(
                    "Pushed up to {} to {remote}/{remote_branch}",
                    target.short_sha()
                ),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Push failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn create_patch(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    let Some(directory) = target
        .repository
        .upgrade()
        .map(|repository| repository.read(cx).work_directory_abs_path.to_path_buf())
    else {
        return;
    };
    let path = cx.prompt_for_new_path(&directory, Some(&format!("{}.patch", target.short_sha())));
    window
        .spawn(cx, async move |cx| {
            let Some(path) = path.await?? else {
                return Ok(());
            };
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            anyhow::ensure!(
                parent_count(&repository, target.sha, cx).await? <= 1,
                "{} is a merge commit; a patch can only be created for a regular commit.",
                target.short_sha()
            );
            // Git writes the file itself: its output read as text would replace bytes of files
            // that are not UTF-8, and the patch would no longer apply.
            git(
                &repository,
                "Creating patch",
                vec![
                    "format-patch".to_string(),
                    "-1".to_string(),
                    format!("--output={}", path.display()),
                    target.sha.to_string(),
                ],
                HashMap::default(),
                cx,
            )
            .await?;
            show_toast(
                &target.workspace,
                format!("Saved the patch to {}", path.display()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Create patch failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn edit_commit_message(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let head = read_head(&repository, cx).await?;
            let details = repository
                .update(cx, |repository, _| repository.show(target.sha.to_string()))
                .await??;
            let workspace = target
                .workspace
                .upgrade()
                .context("the workspace was closed")?;
            workspace.update_in(cx, |workspace, window, cx| {
                let target = target.clone();
                workspace.toggle_modal(window, cx, |window, cx| {
                    CommitMessageModal::new(target, head, details.message, window, cx)
                });
            })?;
            Ok(())
        })
        .detach_and_prompt_err("Edit commit message failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

/// Replaces the message of `target`. `head` is HEAD when the message dialog
/// opened; the rewrite fails if HEAD moved since.
fn reword(
    target: CommitTarget,
    head: HeadState,
    message: String,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            ensure_head_unchanged(&repository, &head, cx).await?;
            if head.sha == target.sha.to_string() {
                ensure_no_operation_in_progress(&repository, cx).await?;
                if !confirm_rewriting_pushed_commit(&target, &repository, "Rewriting", cx).await? {
                    return Ok(());
                }
                git_at_head(
                    &repository,
                    "Editing commit message",
                    args([
                        "-c",
                        "commit.cleanup=whitespace",
                        "commit",
                        "--amend",
                        "--only",
                        "--allow-empty",
                        "-m",
                        &message,
                    ]),
                    HashMap::default(),
                    &head,
                    cx,
                )
                .await?;
            } else {
                let Some(base) = prepare_history_rewrite(&target, &repository, &head, cx).await?
                else {
                    return Ok(());
                };
                let env = HashMap::from_iter([
                    (
                        "GIT_SEQUENCE_EDITOR".to_string(),
                        REWORD_SEQUENCE_EDITOR.to_string(),
                    ),
                    ("GIT_EDITOR".to_string(), MESSAGE_EDITOR.to_string()),
                    ("EVB_COMMIT_MESSAGE".to_string(), message),
                    ("EVB_REWORD_SHA".to_string(), target.sha.to_string()),
                ]);
                rebase(
                    &repository,
                    "Editing commit message",
                    args([
                        "-i",
                        "--autostash",
                        "--no-autosquash",
                        "--no-rebase-merges",
                        &base,
                    ]),
                    env,
                    &head,
                    &replayed_range(&base, &head.sha),
                    cx,
                )
                .await?;
            }
            show_toast(
                &target.workspace,
                format!("Edited the message of {}", target.short_sha()),
                cx,
            );
            Ok(())
        })
        .detach_and_prompt_err("Edit commit message failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

pub(crate) fn new_branch(target: &CommitTarget, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    let title = format!("New Branch from {}", target.short_sha());
    prompt_for_text(
        &target.workspace.clone(),
        title,
        IconName::GitBranch,
        "Branch name",
        None,
        Some("The new branch is checked out.".into()),
        move |name, window, cx| {
            target.run(
                format!("Creating {name}"),
                args(["switch", "-c", &name, &target.sha.to_string()]),
                format!("Created and checked out '{name}' at {}", target.short_sha()),
                "Create branch failed",
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// Adds WebStorm's per-branch submenu ("Branch 'main'") for a branch at the commit.
pub(crate) fn branch_submenu(
    menu: ContextMenu,
    target: &CommitTarget,
    branch: &Branch,
    current_branch: Option<&str>,
) -> ContextMenu {
    let target = target.clone();
    let branch = branch.clone();
    let current_branch = current_branch.map(str::to_string);
    menu.submenu(format!("Branch '{}'", branch.name()), move |menu, _, _| {
        branch_actions(menu, &target, &branch, current_branch.as_deref())
    })
}

/// The actions of WebStorm's branch menus, for `branch` whose tip is `target`.
pub(crate) fn branch_actions(
    mut menu: ContextMenu,
    target: &CommitTarget,
    branch: &Branch,
    current_branch: Option<&str>,
) -> ContextMenu {
    let name = branch.name().to_string();
    // Revisions use the full ref: a local `origin/main`, the remote-tracking
    // `origin/main` and a tag can share a short name.
    let ref_name = branch.ref_name.to_string();
    let is_remote = branch.is_remote();
    let is_current = !is_remote && current_branch == Some(name.as_str());
    if !is_current {
        let target = target.clone();
        let branch = branch.clone();
        menu = menu.entry("Checkout", None, move |window, cx| {
            checkout_branch(&target, &branch, window, cx)
        });
    }
    menu = menu.entry(format!("New Branch from '{name}'…"), None, {
        let target = target.clone();
        move |window, cx| new_branch(&target, window, cx)
    });
    if !is_current {
        let merge_target = target.clone();
        let merge_name = name.clone();
        let merge_ref = ref_name.clone();
        menu = menu
            .entry(
                format!("Merge '{name}' into Current"),
                None,
                move |window, cx| {
                    merge_target.run(
                        format!("Merging {merge_name}"),
                        args(["merge", "--no-edit", &merge_ref]),
                        format!("Merged '{merge_name}'"),
                        "Merge failed",
                        window,
                        cx,
                    )
                },
            )
            .entry(format!("Rebase Current onto '{name}'"), None, {
                let target = target.clone();
                let name = name.clone();
                move |window, cx| rebase_onto(&target, &name, &ref_name, window, cx)
            });
    }
    if !is_remote && !is_current {
        let target = target.clone();
        menu = menu.separator().entry("Delete", None, move |window, cx| {
            delete_branch(&target, &name, window, cx)
        });
    }
    menu
}

fn rebase_onto(
    target: &CommitTarget,
    name: &str,
    ref_name: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let target = target.clone();
    let name = name.to_string();
    let ref_name = ref_name.to_string();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let head = read_head(&repository, cx).await?;
            // Resolved once: the checks and the rebase must see the same destination.
            let onto = git(
                &repository,
                "Checking history",
                args(["rev-parse", "--verify", &format!("{ref_name}^{{commit}}")]),
                HashMap::default(),
                cx,
            )
            .await?
            .trim()
            .to_string();
            let replayed = format!("{onto}..{}", head.sha);
            let merges = git(
                &repository,
                "Checking history",
                args(["rev-list", "--merges", &replayed, "--"]),
                HashMap::default(),
                cx,
            )
            .await?;
            // A rebase flattens merges, dropping changes made only in a merge commit.
            anyhow::ensure!(
                merges.trim().is_empty(),
                "The commits to rebase include merge commits, which can't be rebased here."
            );
            let oldest_replayed = git(
                &repository,
                "Checking history",
                args(["rev-list", "--reverse", &replayed, "--"]),
                HashMap::default(),
                cx,
            )
            .await?;
            if let Some(oldest_replayed) = oldest_replayed.lines().next().map(str::trim)
                && !confirm_rewriting_pushed(oldest_replayed, &repository, "Rebasing", cx).await?
            {
                return Ok(());
            }
            rebase(
                &repository,
                &format!("Rebasing onto {name}"),
                args(["--autostash", &onto]),
                HashMap::default(),
                &head,
                // Both sides: the destination's commits are checked out too.
                &format!("{onto}...{}", head.sha),
                cx,
            )
            .await?;
            show_toast(&target.workspace, format!("Rebased onto '{name}'"), cx);
            Ok(())
        })
        .detach_and_prompt_err("Rebase failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

/// Checks out a local branch, or for a remote branch the local branch that
/// tracks it, creating one when there is none. When a local branch of the
/// same name tracks something else, asks for another name.
fn checkout_branch(target: &CommitTarget, branch: &Branch, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    let branch = branch.clone();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let name = branch.name().to_string();
            let command = if branch.is_remote() {
                let local_name = branch
                    .remote_name()
                    .and_then(|remote| name.strip_prefix(&format!("{remote}/")))
                    .unwrap_or(&name)
                    .to_string();
                let local_upstream = git(
                    &repository,
                    "Checking branches",
                    args([
                        "for-each-ref",
                        "--format=%(upstream)",
                        &format!("refs/heads/{local_name}"),
                    ]),
                    HashMap::default(),
                    cx,
                )
                .await?;
                let local_upstream = local_upstream.trim();
                let local_exists = git(
                    &repository,
                    "Checking branches",
                    args([
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{local_name}"),
                    ]),
                    HashMap::default(),
                    cx,
                )
                .await
                .is_ok();
                if !local_exists {
                    args(["switch", "-c", &local_name, "--track", &branch.ref_name])
                } else if local_upstream == branch.ref_name.as_ref() {
                    args(["switch", "--", &local_name])
                } else {
                    let note = if local_upstream.is_empty() {
                        format!("A local '{local_name}' already exists and tracks no branch.")
                    } else {
                        format!(
                            "A local '{local_name}' already exists and tracks {}.",
                            local_upstream
                                .strip_prefix("refs/remotes/")
                                .unwrap_or(local_upstream)
                        )
                    };
                    let suggested =
                        format!("{local_name}-{}", branch.remote_name().unwrap_or("remote"));
                    cx.update(|window, cx| {
                        let workspace = target.workspace.clone();
                        prompt_for_text(
                            &workspace,
                            format!("Check Out '{name}' as"),
                            IconName::GitBranch,
                            "Local branch name",
                            Some(&suggested),
                            Some(note.into()),
                            move |local_name, window, cx| {
                                target.run(
                                    format!("Checking out {name}"),
                                    args([
                                        "switch",
                                        "-c",
                                        &local_name,
                                        "--track",
                                        &branch.ref_name,
                                    ]),
                                    format!("Checked out '{name}' as '{local_name}'"),
                                    "Checkout failed",
                                    window,
                                    cx,
                                );
                            },
                            window,
                            cx,
                        )
                    })?;
                    return Ok(());
                }
            } else {
                args(["switch", "--", &name])
            };
            git(
                &repository,
                &format!("Checking out {name}"),
                command,
                HashMap::default(),
                cx,
            )
            .await?;
            show_toast(&target.workspace, format!("Checked out '{name}'"), cx);
            Ok(())
        })
        .detach_and_prompt_err("Checkout failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

fn delete_branch(target: &CommitTarget, name: &str, window: &mut Window, cx: &mut App) {
    let target = target.clone();
    let name = name.to_string();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            if !confirm(
                PromptLevel::Warning,
                format!("Delete branch '{name}'?"),
                None,
                "Delete",
                cx,
            )
            .await?
            {
                return Ok(());
            }
            let deleted = git(
                &repository,
                &format!("Deleting {name}"),
                args(["branch", "-d", "--", &name]),
                // Its message is matched below, so it must not be translated.
                HashMap::from_iter([("LC_ALL".to_string(), "C".to_string())]),
                cx,
            )
            .await;
            if let Err(error) = deleted {
                if !format!("{error:#}").contains("not fully merged") {
                    return Err(error);
                }
                if !confirm(
                    PromptLevel::Warning,
                    format!("'{name}' is not fully merged"),
                    Some("Its unmerged commits will be lost. Delete it anyway?".to_string()),
                    "Delete",
                    cx,
                )
                .await?
                {
                    return Ok(());
                }
                git(
                    &repository,
                    &format!("Deleting {name}"),
                    args(["branch", "-D", "--", &name]),
                    HashMap::default(),
                    cx,
                )
                .await?;
            }
            show_toast(&target.workspace, format!("Deleted branch '{name}'"), cx);
            Ok(())
        })
        .detach_and_prompt_err("Delete branch failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

/// Opens a one-line input dialog and passes the trimmed, non-empty text to `on_confirm`.
pub(crate) fn prompt_for_text(
    workspace: &WeakEntity<Workspace>,
    title: impl Into<SharedString>,
    icon: IconName,
    placeholder: &str,
    initial_text: Option<&str>,
    note: Option<SharedString>,
    on_confirm: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let title = title.into();
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                if let Some(text) = initial_text {
                    editor.set_text(text, window, cx);
                    editor.select_all(&editor::actions::SelectAll, window, cx);
                }
                editor
            });
            TextInputModal {
                title,
                icon,
                note,
                editor,
                on_confirm: Box::new(on_confirm),
            }
        });
    });
}

struct TextInputModal {
    title: SharedString,
    icon: IconName,
    note: Option<SharedString>,
    editor: Entity<Editor>,
    on_confirm: Box<dyn Fn(String, &mut Window, &mut App)>,
}

impl TextInputModal {
    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        (self.on_confirm)(text, window, cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for TextInputModal {}
impl ModalView for TextInputModal {}
impl Focusable for TextInputModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for TextInputModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TextInputModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(self.icon).size(IconSize::XSmall))
                    .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall)),
            )
            .child(div().px_3().pb_2().child(self.editor.clone()))
            .when_some(self.note.clone(), |this, note| {
                this.child(
                    div()
                        .px_3()
                        .pb_2()
                        .child(Label::new(note).size(LabelSize::Small).color(Color::Muted)),
                )
            })
    }
}

struct CommitMessageModal {
    target: CommitTarget,
    head: HeadState,
    editor: Entity<Editor>,
}

impl CommitMessageModal {
    fn new(
        target: CommitTarget,
        head: HeadState,
        message: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 16, window, cx);
            editor.set_text(message.trim_end(), window, cx);
            editor
        });
        Self {
            target,
            head,
            editor,
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.editor.read(cx).text(cx).trim().to_string();
        if message.is_empty() {
            return;
        }
        reword(self.target.clone(), self.head.clone(), message, window, cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for CommitMessageModal {}
impl ModalView for CommitMessageModal {}
impl Focusable for CommitMessageModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for CommitMessageModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CommitMessageModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(40.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitCommit).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!(
                            "Edit Commit Message of {}",
                            self.target.short_sha()
                        ))
                        .size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                div()
                    .mx_3()
                    .p_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_end()
                    .gap_1()
                    .child(Button::new("cancel-commit-message", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("save-commit-message", "Save")
                            .style(ButtonStyle::Filled)
                            .key_binding(KeyBinding::for_action_in(
                                &Confirm,
                                &self.editor.focus_handle(cx),
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&Confirm, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fs::RealFs;
    use gpui::{TestAppContext, VisualTestContext};
    use language::language_settings::AllLanguageSettings;
    use project::{Project, WorktreeSettings, project_settings::ProjectSettings};
    use serde_json::json;
    use settings::{Settings as _, SettingsStore};
    use std::{path::Path, time::Duration};
    use theme::LoadThemes;
    use util::test::TempTree;
    use workspace::WorkspaceSettings;

    pub(crate) fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            AllLanguageSettings::register(cx);
            editor::init(cx);
            ProjectSettings::register(cx);
            WorktreeSettings::register(cx);
            WorkspaceSettings::register(cx);
        });
    }

    #[allow(clippy::disallowed_methods)]
    pub(crate) fn try_run_git(directory: &Path, arguments: &[&str]) -> Result<String, String> {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(arguments)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[track_caller]
    pub(crate) fn run_git(directory: &Path, arguments: &[&str]) -> String {
        try_run_git(directory, arguments)
            .unwrap_or_else(|error| panic!("git {arguments:?} failed: {error}"))
    }

    pub(crate) fn commit_file(
        directory: &Path,
        file: &str,
        contents: &str,
        message: &str,
    ) -> String {
        std::fs::write(directory.join(file), contents).expect("writing a fixture file");
        run_git(directory, &["add", file]);
        run_git(directory, &["commit", "-q", "-m", message]);
        run_git(directory, &["rev-parse", "HEAD"])
    }

    fn subjects(directory: &Path) -> Vec<String> {
        run_git(directory, &["log", "--format=%s"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Creates `main` with commits A, B and C (one file each). The repository's
    /// own config pins the identity and turns signing off, since operations run
    /// with the user's global git config.
    pub(crate) fn create_repository(root: &Path) -> Vec<String> {
        run_git(root, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            run_git(root, &["config", key, value]);
        }
        ["a", "b", "c"]
            .into_iter()
            .map(|name| commit_file(root, &format!("{name}.txt"), "1\n", &name.to_uppercase()))
            .collect()
    }

    pub(crate) async fn open(
        root: &Path,
        cx: &mut TestAppContext,
    ) -> (VisualTestContext, Entity<Repository>, WeakEntity<Workspace>) {
        let project = Project::test(RealFs::new(None, cx.executor()), [root], cx).await;
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let workspace = window
            .update(cx, |_, _, cx| cx.weak_entity())
            .expect("reading the workspace");
        let mut cx = VisualTestContext::from_window(*window, cx);
        let repository = wait_for(&mut cx, |cx| project.read(cx).active_repository(cx));
        (cx, repository, workspace)
    }

    /// Pumps the app until `condition` returns a value; git runs in real processes.
    #[track_caller]
    pub(crate) fn wait_for<T>(
        cx: &mut VisualTestContext,
        mut condition: impl FnMut(&App) -> Option<T>,
    ) -> T {
        for _ in 0..3000 {
            cx.run_until_parked();
            if let Some(value) = cx.update(|_, cx| condition(cx)) {
                return value;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for a git operation");
    }

    fn wait_for_prompt(cx: &mut VisualTestContext, answer: &str) {
        for _ in 0..3000 {
            cx.run_until_parked();
            if cx.has_pending_prompt() {
                cx.simulate_prompt_answer(answer);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for a prompt");
    }

    fn head_state(directory: &Path) -> HeadState {
        HeadState {
            sha: run_git(directory, &["rev-parse", "HEAD"]),
            branch: try_run_git(directory, &["symbolic-ref", "-q", "HEAD"]).ok(),
        }
    }

    /// Waits until the operation started last has stopped changing the repository.
    fn target(
        sha: &str,
        repository: &Entity<Repository>,
        workspace: &WeakEntity<Workspace>,
    ) -> CommitTarget {
        CommitTarget {
            sha: sha.parse().expect("a valid sha"),
            repository: repository.downgrade(),
            workspace: workspace.clone(),
        }
    }

    #[gpui::test]
    async fn test_edit_commit_message(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        let (mut cx, repository, workspace) = open(root, cx).await;

        let message = "B edited\n\n#1 lines starting with a hash are kept".to_string();
        cx.update(|window, cx| {
            reword(
                target(&shas[1], &repository, &workspace),
                head_state(root),
                message,
                window,
                cx,
            )
        });
        // Mid-rebase the log is shorter, so compare whole lists.
        wait_for(&mut cx, |_| {
            (subjects(root) == ["C", "B edited", "A"]).then_some(())
        });
        assert_eq!(subjects(root), ["C", "B edited", "A"]);
        assert_eq!(
            run_git(root, &["log", "-1", "--format=%b", "HEAD~1"]),
            "#1 lines starting with a hash are kept"
        );
        assert_eq!(run_git(root, &["show", "HEAD:c.txt"]), "1");

        let head = run_git(root, &["rev-parse", "HEAD"]);
        cx.update(|window, cx| {
            reword(
                target(&head, &repository, &workspace),
                head_state(root),
                "C edited".into(),
                window,
                cx,
            )
        });
        wait_for(&mut cx, |_| {
            (subjects(root) == ["C edited", "B edited", "A"]).then_some(())
        });
        assert_eq!(subjects(root), ["C edited", "B edited", "A"]);
        assert_eq!(run_git(root, &["status", "--porcelain"]), "");
    }

    #[gpui::test]
    async fn test_drop_and_fixup(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        let (mut cx, repository, workspace) = open(root, cx).await;

        std::fs::write(root.join("a.txt"), "2\n").expect("editing a.txt");
        run_git(root, &["add", "a.txt"]);
        cx.update(|window, cx| fixup(&target(&shas[0], &repository, &workspace), window, cx));
        wait_for(&mut cx, |_| {
            // Mid-rebase HEAD~2 may not exist yet.
            (try_run_git(root, &["show", "HEAD~2:a.txt"]).as_deref() == Ok("2")).then_some(())
        });
        assert_eq!(subjects(root), ["C", "B", "A"]);
        assert_eq!(run_git(root, &["status", "--porcelain"]), "");

        // An unstaged change survives the rebase that drops B.
        std::fs::write(root.join("c.txt"), "local\n").expect("editing c.txt");
        let b = run_git(root, &["rev-parse", "HEAD~1"]);
        cx.update(|window, cx| drop_commit(&target(&b, &repository, &workspace), window, cx));
        wait_for_prompt(&mut cx, "Drop");
        // The rebase restores the autostashed change after rewriting history.
        wait_for(&mut cx, |_| {
            (subjects(root).len() == 2
                && !root.join(".git/rebase-merge").exists()
                && std::fs::read_to_string(root.join("c.txt")).ok().as_deref() == Some("local\n"))
            .then_some(())
        });
        assert_eq!(subjects(root), ["C", "A"]);
        assert!(!root.join("b.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("c.txt")).expect("reading c.txt"),
            "local\n"
        );
    }

    #[gpui::test]
    async fn test_revert_and_undo(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        let (mut cx, repository, workspace) = open(root, cx).await;

        cx.update(|window, cx| revert(&target(&shas[1], &repository, &workspace), window, cx));
        wait_for(&mut cx, |_| (subjects(root).len() == 4).then_some(()));
        assert_eq!(subjects(root)[0], "Revert \"B\"");
        assert!(!root.join("b.txt").exists());

        let head = run_git(root, &["rev-parse", "HEAD"]);
        cx.update(|window, cx| undo_commit(&target(&head, &repository, &workspace), window, cx));
        wait_for(&mut cx, |_| (subjects(root).len() == 3).then_some(()));
        assert_eq!(subjects(root), ["C", "B", "A"]);
        assert_eq!(run_git(root, &["status", "--porcelain"]), "D  b.txt");
    }

    #[gpui::test]
    async fn test_push_up_to_commit(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({ "repo": {}, "origin.git": {} }));
        let root = tree.path().join("repo");
        let origin = tree.path().join("origin.git");
        run_git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let shas = create_repository(&root);
        run_git(
            &root,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        run_git(
            &root,
            &[
                "push",
                "-q",
                "-u",
                "origin",
                &format!("{}:refs/heads/main", shas[0]),
            ],
        );
        run_git(&root, &["branch", "--set-upstream-to", "origin/main"]);
        let (mut cx, repository, workspace) = open(&root, cx).await;

        cx.update(|window, cx| push_up_to(&target(&shas[1], &repository, &workspace), window, cx));
        wait_for(&mut cx, |_| {
            (try_run_git(&origin, &["rev-parse", "main"]).as_ref() == Ok(&shas[1])).then_some(())
        });

        // Rewriting a pushed commit asks first; cancelling leaves history alone.
        cx.update(|window, cx| {
            reword(
                target(&shas[1], &repository, &workspace),
                head_state(&root),
                "B2".into(),
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "Cancel");
        cx.run_until_parked();
        assert_eq!(subjects(&root), ["C", "B", "A"]);
    }

    #[gpui::test]
    async fn test_reword_with_unusual_rebase_settings(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        // Both change the todo list git generates.
        run_git(root, &["config", "rebase.abbreviateCommands", "true"]);
        run_git(root, &["config", "rebase.rebaseMerges", "true"]);
        let (mut cx, repository, workspace) = open(root, cx).await;

        cx.update(|window, cx| {
            reword(
                target(&shas[0], &repository, &workspace),
                head_state(root),
                "A edited".into(),
                window,
                cx,
            )
        });
        wait_for(&mut cx, |_| {
            (subjects(root) == ["C", "B", "A edited"]).then_some(())
        });
    }

    #[gpui::test]
    async fn test_operations_leave_a_stopped_rebase_alone(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        // Stop a rebase on a conflict: `side` changes c.txt like C does.
        run_git(root, &["switch", "-q", "-c", "side", &shas[1]]);
        commit_file(root, "c.txt", "side\n", "side C");
        let conflict = try_run_git(root, &["rebase", "main"]);
        assert!(
            conflict.is_err(),
            "the fixture rebase should stop on a conflict"
        );
        let rebase_head = run_git(root, &["rev-parse", "HEAD"]);
        let (mut cx, repository, workspace) = open(root, cx).await;

        let branch = Branch {
            is_head: false,
            ref_name: "refs/heads/main".into(),
            upstream: None,
            most_recent_commit: None,
        };
        // Each operation must refuse, with its error, before anything is checked.
        cx.update(|window, cx| {
            rebase_onto(
                &target(&shas[2], &repository, &workspace),
                branch.name(),
                &branch.ref_name,
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "OK");
        cx.update(|window, cx| drop_commit(&target(&shas[1], &repository, &workspace), window, cx));
        wait_for_prompt(&mut cx, "OK");
        assert!(
            root.join(".git/rebase-merge").exists(),
            "the stopped rebase must not be aborted"
        );
        assert_eq!(run_git(root, &["rev-parse", "HEAD"]), rebase_head);
    }

    #[gpui::test]
    async fn test_failed_drop_aborts_its_own_rebase(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        // D edits b.txt, so dropping B conflicts.
        let d = commit_file(root, "b.txt", "2\n", "D");
        let (mut cx, repository, workspace) = open(root, cx).await;

        cx.update(|window, cx| drop_commit(&target(&shas[1], &repository, &workspace), window, cx));
        wait_for_prompt(&mut cx, "Drop");
        wait_for_prompt(&mut cx, "OK");
        assert!(
            !root.join(".git/rebase-merge").exists(),
            "the failed rebase is aborted"
        );
        assert_eq!(run_git(root, &["rev-parse", "HEAD"]), d);
        assert_eq!(
            try_run_git(root, &["symbolic-ref", "--short", "HEAD"]).as_deref(),
            Ok("main")
        );
    }

    #[gpui::test]
    async fn test_operations_refuse_a_moved_head(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        let (mut cx, repository, workspace) = open(root, cx).await;

        // The message dialog opened on C, then another commit was made.
        let stale_head = head_state(root);
        let d = commit_file(root, "d.txt", "1\n", "D");
        cx.update(|window, cx| {
            reword(
                target(&shas[2], &repository, &workspace),
                stale_head,
                "C edited".into(),
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "OK");
        assert_eq!(subjects(root), ["D", "C", "B", "A"]);

        // The reset dialog is answered after HEAD moved to another branch.
        run_git(root, &["branch", "other", &shas[0]]);
        cx.update(|window, cx| {
            reset_current_branch(&target(&shas[0], &repository, &workspace), window, cx)
        });
        for _ in 0..3000 {
            cx.run_until_parked();
            if cx.has_pending_prompt() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        run_git(root, &["switch", "-q", "other"]);
        cx.simulate_prompt_answer("Hard");
        wait_for_prompt(&mut cx, "OK");
        assert_eq!(run_git(root, &["rev-parse", "main"]), d);
        assert_eq!(run_git(root, &["rev-parse", "other"]), shas[0]);
    }

    #[gpui::test]
    async fn test_rewrites_keep_staged_changes_and_ignored_files(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        // `local-data` was committed once and then untracked; the local copy is ignored now.
        // A name git quotes in its line output.
        commit_file(
            root,
            ".gitignore",
            "local-data\nданные.txt\n",
            "Ignore local data",
        );
        std::fs::write(root.join("local-data"), "old\n").expect("writing a fixture file");
        std::fs::write(root.join("данные.txt"), "old\n").expect("writing a fixture file");
        run_git(root, &["add", "-f", "данные.txt"]);
        run_git(root, &["commit", "-q", "-m", "Add quoted data"]);
        let with_quoted_data = run_git(root, &["rev-parse", "HEAD"]);
        run_git(root, &["rm", "-q", "--cached", "данные.txt"]);
        run_git(root, &["commit", "-q", "-m", "Stop tracking quoted data"]);
        std::fs::write(root.join("данные.txt"), "valuable\n").expect("writing a fixture file");
        run_git(root, &["add", "-f", "local-data"]);
        run_git(root, &["commit", "-q", "-m", "Add local data"]);
        let with_data = run_git(root, &["rev-parse", "HEAD"]);
        run_git(root, &["rm", "-q", "--cached", "local-data"]);
        run_git(root, &["commit", "-q", "-m", "Stop tracking local data"]);
        std::fs::write(root.join("local-data"), "valuable\n").expect("writing a fixture file");
        let (mut cx, repository, workspace) = open(root, cx).await;

        // Rewriting the commit that wrote `local-data` would overwrite, then delete, it.
        cx.update(|window, cx| {
            reword(
                target(&with_data, &repository, &workspace),
                head_state(root),
                "Edited".into(),
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "OK");
        assert_eq!(
            std::fs::read_to_string(root.join("local-data")).unwrap(),
            "valuable\n"
        );
        cx.update(|window, cx| {
            reword(
                target(&with_quoted_data, &repository, &workspace),
                head_state(root),
                "Edited".into(),
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "OK");
        assert_eq!(
            std::fs::read_to_string(root.join("данные.txt")).unwrap(),
            "valuable\n"
        );

        // A staged change would come back unstaged, losing what was staged.
        std::fs::write(root.join("a.txt"), "staged\n").expect("writing a fixture file");
        run_git(root, &["add", "a.txt"]);
        std::fs::write(root.join("a.txt"), "unstaged\n").expect("writing a fixture file");
        cx.update(|window, cx| {
            reword(
                target(&shas[1], &repository, &workspace),
                head_state(root),
                "B edited".into(),
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "OK");
        assert_eq!(run_git(root, &["show", ":a.txt"]), "staged");
        assert!(!subjects(root).contains(&"B edited".to_string()));
    }

    #[gpui::test]
    async fn test_head_checked_command_refuses_a_moved_head(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        let (mut cx, repository, _) = open(root, cx).await;

        // Confirmed on `main` at C, but a queued switch moved HEAD to `other` first.
        run_git(root, &["switch", "-q", "-c", "other"]);
        let mut receiver = repository.update(&mut cx, |repository, cx| {
            repository.run_git_command_at_head(
                None,
                args(["reset", "--hard", &shas[0]]),
                HashMap::default(),
                shas[2].clone(),
                Some("refs/heads/main".into()),
                cx,
            )
        });
        let result = loop {
            cx.run_until_parked();
            if let Ok(Some(result)) = receiver.try_recv() {
                break result;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(result.is_err());
        assert_eq!(run_git(root, &["rev-parse", "other"]), shas[2]);
        assert_eq!(run_git(root, &["rev-parse", "main"]), shas[2]);
    }

    #[gpui::test]
    async fn test_rebase_onto_refuses_merge_commits(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        let shas = create_repository(root);
        // `main` gets a merge that adds a file of its own; `onto` diverges from A.
        run_git(root, &["switch", "-q", "-c", "side", &shas[1]]);
        commit_file(root, "side.txt", "1\n", "S");
        run_git(root, &["switch", "-q", "main"]);
        run_git(root, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
        std::fs::write(root.join("resolution.txt"), "1\n").expect("writing a fixture file");
        run_git(root, &["add", "resolution.txt"]);
        run_git(root, &["commit", "-q", "-m", "Merge side"]);
        let merged = run_git(root, &["rev-parse", "HEAD"]);
        run_git(root, &["branch", "onto", &shas[0]]);
        run_git(root, &["switch", "-q", "onto"]);
        commit_file(root, "o.txt", "1\n", "O");
        run_git(root, &["switch", "-q", "main"]);
        let (mut cx, repository, workspace) = open(root, cx).await;

        cx.update(|window, cx| {
            rebase_onto(
                &target(&shas[0], &repository, &workspace),
                "onto",
                "refs/heads/onto",
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "OK");
        assert_eq!(run_git(root, &["rev-parse", "main"]), merged);
    }

    #[gpui::test]
    async fn test_push_up_to_commit_uses_the_push_remote(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({ "repo": {}, "upstream.git": {}, "fork.git": {} }));
        let root = tree.path().join("repo");
        let upstream = tree.path().join("upstream.git");
        let fork = tree.path().join("fork.git");
        for bare in [&upstream, &fork] {
            run_git(bare, &["init", "-q", "--bare", "-b", "main"]);
        }
        let shas = create_repository(&root);
        run_git(
            &root,
            &["remote", "add", "upstream", &upstream.to_string_lossy()],
        );
        run_git(&root, &["remote", "add", "fork", &fork.to_string_lossy()]);
        run_git(
            &root,
            &[
                "push",
                "-q",
                "upstream",
                &format!("{}:refs/heads/main", shas[0]),
            ],
        );
        run_git(&root, &["branch", "--set-upstream-to", "upstream/main"]);
        run_git(&root, &["config", "branch.main.pushRemote", "fork"]);
        let (mut cx, repository, workspace) = open(&root, cx).await;

        cx.update(|window, cx| push_up_to(&target(&shas[1], &repository, &workspace), window, cx));
        wait_for(&mut cx, |_| {
            (try_run_git(&fork, &["rev-parse", "main"]).as_ref() == Ok(&shas[1])).then_some(())
        });
        assert_eq!(run_git(&upstream, &["rev-parse", "main"]), shas[0]);

        // A push refspec of the push remote picks the destination branch.
        run_git(
            &root,
            &[
                "config",
                "remote.fork.push",
                "refs/heads/main:refs/heads/review",
            ],
        );
        cx.update(|window, cx| push_up_to(&target(&shas[2], &repository, &workspace), window, cx));
        wait_for(&mut cx, |_| {
            (try_run_git(&fork, &["rev-parse", "review"]).as_ref() == Ok(&shas[2])).then_some(())
        });
        assert_eq!(run_git(&fork, &["rev-parse", "main"]), shas[1]);
    }
}

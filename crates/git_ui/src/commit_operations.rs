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

/// Replaces the first `pick` of an interactive rebase todo list with `reword`.
const REWORD_SEQUENCE_EDITOR: &str =
    r#"sh -c 'sed "1s/^pick /reword /" "$1" > "$1.evb" && mv "$1.evb" "$1"' evb-sequence-editor"#;
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

    fn current_branch_name(&self, cx: &App) -> Option<SharedString> {
        let repository = self.repository.upgrade()?;
        let repository = repository.read(cx);
        repository
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_string()))
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
    let status = status.to_string();
    repository
        .update(cx, |repository, cx| {
            repository.run_git_command(status, args, env, cx)
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

fn args<const N: usize>(args: [&str; N]) -> Vec<String> {
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

/// Checks that rewriting history from `target` is possible with a linear
/// rebase and asks before rewriting pushed commits. Returns the rebase base
/// argument, or `None` when the user cancelled.
async fn prepare_history_rewrite(
    target: &CommitTarget,
    repository: &Entity<Repository>,
    cx: &mut AsyncWindowContext,
) -> Result<Option<String>> {
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
        args(["merge-base", "--is-ancestor", &sha, "HEAD"]),
        HashMap::default(),
        cx,
    )
    .await
    .map_err(|_| anyhow::anyhow!("{short_sha} is not in the current branch."))?;
    let merges = git(
        repository,
        "Checking history",
        args(["rev-list", "--merges", &format!("{sha}..HEAD")]),
        HashMap::default(),
        cx,
    )
    .await?;
    anyhow::ensure!(
        merges.trim().is_empty(),
        "The commits after {short_sha} include merge commits, which can't be rewritten here."
    );
    let pushed_to = git(
        repository,
        "Checking history",
        args([
            "branch",
            "-r",
            "--contains",
            &sha,
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
    if !pushed_to.is_empty()
        && !confirm(
            PromptLevel::Warning,
            format!("{short_sha} is already pushed to {}", pushed_to.join(", ")),
            Some("Rewriting it means you will have to force push. Continue?".to_string()),
            "Continue",
            cx,
        )
        .await?
    {
        return Ok(None);
    }
    Ok(Some(if parent_count == 0 {
        "--root".to_string()
    } else {
        format!("{sha}^")
    }))
}

/// Runs a rebase and aborts it when it fails, so a failed rewrite leaves the
/// branch as it was.
async fn rebase(
    repository: &Entity<Repository>,
    status: &str,
    rebase_args: Vec<String>,
    env: HashMap<String, String>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let mut command = args(["-c", "commit.cleanup=whitespace", "rebase"]);
    command.extend(rebase_args);
    let result = git(repository, status, command, env, cx).await;
    if let Err(error) = result {
        git(
            repository,
            "Aborting rebase",
            args(["rebase", "--abort"]),
            HashMap::default(),
            cx,
        )
        .await
        .log_err();
        return Err(error.context("The rebase failed and was aborted"));
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
            let mut command = args([subcommand]);
            if subcommand == "revert" {
                command.push("--no-edit".to_string());
            }
            if parent_count(&repository, target.sha, cx).await? > 1 {
                command.extend(args(["-m", "1"]));
            }
            command.push(target.sha.to_string());
            git(
                &repository,
                &format!("{status} {}", target.short_sha()),
                command,
                HashMap::default(),
                cx,
            )
            .await?;
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
    let branch = target
        .current_branch_name(cx)
        .map(|name| format!("'{name}'"))
        .unwrap_or_else(|| "HEAD".to_string());
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
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
            git(
                &repository,
                &format!("Resetting to {}", target.short_sha()),
                args(["reset", &flag, &target.sha.to_string()]),
                HashMap::default(),
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
            anyhow::ensure!(
                parent_count(&repository, target.sha, cx).await? == 1,
                "Only a commit with one parent can be undone."
            );
            let pushed_to = git(
                &repository,
                "Checking history",
                args([
                    "branch",
                    "-r",
                    "--contains",
                    &target.sha.to_string(),
                    "--format=%(refname:short)",
                ]),
                HashMap::default(),
                cx,
            )
            .await?;
            if !pushed_to.trim().is_empty()
                && !confirm(
                    PromptLevel::Warning,
                    format!(
                        "{} is already pushed to {}",
                        target.short_sha(),
                        pushed_to.trim().replace('\n', ", ")
                    ),
                    Some("Undo it anyway? You will have to force push.".to_string()),
                    "Undo",
                    cx,
                )
                .await?
            {
                return Ok(());
            }
            git(
                &repository,
                "Undoing commit",
                args(["reset", "--soft", "HEAD^"]),
                HashMap::default(),
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
            if prepare_history_rewrite(&target, &repository, cx)
                .await?
                .is_none()
            {
                return Ok(());
            }
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
                args(["--autostash", "--onto", &format!("{sha}^"), &sha]),
                HashMap::default(),
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
            let Some(base) = prepare_history_rewrite(&target, &repository, cx).await? else {
                return Ok(());
            };
            let sha = target.sha.to_string();
            git(
                &repository,
                "Committing fixup",
                args(["commit", &format!("--fixup={sha}")]),
                HashMap::default(),
                cx,
            )
            .await?;
            rebase(
                &repository,
                &format!("Folding changes into {}", target.short_sha()),
                args(["-i", "--autosquash", "--autostash", &base]),
                HashMap::default(),
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
                args(["symbolic-ref", "--short", "HEAD"]),
                HashMap::default(),
                cx,
            )
            .await
            .map_err(|_| anyhow::anyhow!("HEAD is detached; check out a branch first."))?;
            let branch = branch.trim().to_string();
            let upstream = async |key: &str, cx: &mut AsyncWindowContext| {
                git(
                    &repository,
                    "Reading upstream",
                    args(["config", &format!("branch.{branch}.{key}")]),
                    HashMap::default(),
                    cx,
                )
                .await
                .map(|value| value.trim().to_string())
                .map_err(|_| anyhow::anyhow!("'{branch}' has no upstream branch; push it first."))
            };
            let remote = upstream("remote", cx).await?;
            let merge_ref = upstream("merge", cx).await?;
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
            let patch = git(
                &repository,
                "Creating patch",
                args(["format-patch", "-1", "--stdout", &target.sha.to_string()]),
                HashMap::default(),
                cx,
            )
            .await?;
            let write_path = path.clone();
            cx.background_spawn(async move { std::fs::write(&write_path, patch) })
                .await
                .with_context(|| format!("writing {}", path.display()))?;
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
    let is_head = target.is_head(cx);
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
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
                    CommitMessageModal::new(target, is_head, details.message, window, cx)
                });
            })?;
            Ok(())
        })
        .detach_and_prompt_err("Edit commit message failed", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
}

fn reword(target: CommitTarget, is_head: bool, message: String, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            if is_head {
                git(
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
                    cx,
                )
                .await?;
            } else {
                let Some(base) = prepare_history_rewrite(&target, &repository, cx).await? else {
                    return Ok(());
                };
                let env = HashMap::from_iter([
                    (
                        "GIT_SEQUENCE_EDITOR".to_string(),
                        REWORD_SEQUENCE_EDITOR.to_string(),
                    ),
                    ("GIT_EDITOR".to_string(), MESSAGE_EDITOR.to_string()),
                    ("EVB_COMMIT_MESSAGE".to_string(), message),
                ]);
                rebase(
                    &repository,
                    "Editing commit message",
                    args(["-i", "--autostash", "--no-autosquash", &base]),
                    env,
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
    let Some(workspace) = target.workspace.upgrade() else {
        return;
    };
    let target = target.clone();
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            NewBranchModal::new(target, window, cx)
        });
    });
}

/// Adds WebStorm's per-branch submenu ("Branch 'main'") for a branch at the commit.
pub(crate) fn branch_submenu(
    menu: ContextMenu,
    target: &CommitTarget,
    branch: &Branch,
    current_branch: Option<&str>,
) -> ContextMenu {
    let name = branch.name().to_string();
    let is_remote = branch.is_remote();
    let is_current = !is_remote && current_branch == Some(name.as_str());
    let target = target.clone();
    menu.submenu(format!("Branch '{name}'"), move |mut menu, _, _| {
        if !is_current {
            let target = target.clone();
            let name = name.clone();
            menu = menu.entry("Checkout", None, move |window, cx| {
                checkout_branch(&target, &name, is_remote, window, cx)
            });
        }
        if !is_current {
            let target_for_merge = target.clone();
            let merge_name = name.clone();
            menu = menu
                .entry(
                    format!("Merge '{name}' into Current"),
                    None,
                    move |window, cx| {
                        target_for_merge.run(
                            format!("Merging {merge_name}"),
                            args(["merge", "--no-edit", &merge_name]),
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
                    move |window, cx| {
                        let target = target.clone();
                        let name = name.clone();
                        window
                            .spawn(cx, async move |cx| {
                                let repository = target
                                    .repository
                                    .upgrade()
                                    .context("the repository was closed")?;
                                rebase(
                                    &repository,
                                    &format!("Rebasing onto {name}"),
                                    args(["--autostash", &name]),
                                    HashMap::default(),
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
                });
        }
        if !is_remote && !is_current {
            let target = target.clone();
            let name = name.clone();
            menu = menu.separator().entry("Delete", None, move |window, cx| {
                delete_branch(&target, &name, window, cx)
            });
        }
        menu
    })
}

fn checkout_branch(
    target: &CommitTarget,
    name: &str,
    is_remote: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let target = target.clone();
    let name = name.to_string();
    window
        .spawn(cx, async move |cx| {
            let repository = target
                .repository
                .upgrade()
                .context("the repository was closed")?;
            let mut command = args(["switch", &name]);
            if is_remote {
                // `origin/feature` checks out the local `feature`, creating it to
                // track the remote branch when it doesn't exist yet.
                let local_name = name
                    .split_once('/')
                    .map(|(_, local)| local)
                    .unwrap_or(&name);
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
                command = if local_exists {
                    args(["switch", local_name])
                } else {
                    args(["switch", "--track", &name])
                };
            }
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
                args(["branch", "-d", &name]),
                HashMap::default(),
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
                    args(["branch", "-D", &name]),
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

struct NewBranchModal {
    target: CommitTarget,
    editor: Entity<Editor>,
}

impl NewBranchModal {
    fn new(target: CommitTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Branch name", window, cx);
            editor
        });
        Self { target, editor }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return;
        }
        self.target.run(
            format!("Creating {name}"),
            args(["switch", "-c", &name, &self.target.sha.to_string()]),
            format!(
                "Created and checked out '{name}' at {}",
                self.target.short_sha()
            ),
            "Create branch failed",
            window,
            cx,
        );
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for NewBranchModal {}
impl ModalView for NewBranchModal {}
impl Focusable for NewBranchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for NewBranchModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NewBranchModal")
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
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("New Branch from {}", self.target.short_sha()))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(div().px_3().pb_1().child(self.editor.clone()))
            .child(
                div().px_3().pb_2().child(
                    Label::new("The new branch is checked out.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
    }
}

struct CommitMessageModal {
    target: CommitTarget,
    is_head: bool,
    editor: Entity<Editor>,
}

impl CommitMessageModal {
    fn new(
        target: CommitTarget,
        is_head: bool,
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
            is_head,
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
        reword(self.target.clone(), self.is_head, message, window, cx);
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
mod tests {
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

    fn init_test(cx: &mut TestAppContext) {
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
    #[track_caller]
    fn run_git(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(arguments)
            .output()
            .expect("running git");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_file(directory: &Path, file: &str, contents: &str, message: &str) -> String {
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
    fn create_repository(root: &Path) -> Vec<String> {
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

    async fn open(
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
    fn wait_for<T>(cx: &mut VisualTestContext, mut condition: impl FnMut(&App) -> Option<T>) -> T {
        for _ in 0..500 {
            cx.run_until_parked();
            if let Some(value) = cx.update(|_, cx| condition(cx)) {
                return value;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for a git operation");
    }

    fn wait_for_prompt(cx: &mut VisualTestContext, answer: &str) {
        for _ in 0..500 {
            cx.run_until_parked();
            if cx.has_pending_prompt() {
                cx.simulate_prompt_answer(answer);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for a prompt");
    }

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
                false,
                message,
                window,
                cx,
            )
        });
        wait_for(&mut cx, |_| (subjects(root)[1] == "B edited").then_some(()));
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
                true,
                "C edited".into(),
                window,
                cx,
            )
        });
        wait_for(&mut cx, |_| (subjects(root)[0] == "C edited").then_some(()));
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
            (run_git(root, &["show", "HEAD~2:a.txt"]) == "2").then_some(())
        });
        assert_eq!(subjects(root), ["C", "B", "A"]);
        assert_eq!(run_git(root, &["status", "--porcelain"]), "");

        // An unstaged change survives the rebase that drops B.
        std::fs::write(root.join("c.txt"), "local\n").expect("editing c.txt");
        let b = run_git(root, &["rev-parse", "HEAD~1"]);
        cx.update(|window, cx| drop_commit(&target(&b, &repository, &workspace), window, cx));
        wait_for_prompt(&mut cx, "Drop");
        wait_for(&mut cx, |_| (subjects(root).len() == 2).then_some(()));
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
            (run_git(&origin, &["rev-parse", "main"]) == shas[1]).then_some(())
        });

        // Rewriting a pushed commit asks first; cancelling leaves history alone.
        cx.update(|window, cx| {
            reword(
                target(&shas[1], &repository, &workspace),
                false,
                "B2".into(),
                window,
                cx,
            )
        });
        wait_for_prompt(&mut cx, "Cancel");
        cx.run_until_parked();
        assert_eq!(subjects(&root), ["C", "B", "A"]);
    }
}

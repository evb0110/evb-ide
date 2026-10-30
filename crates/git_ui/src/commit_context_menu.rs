use crate::{
    commit_operations::{self, CommitTarget},
    commit_view::CommitView,
    create_tag_at_commit,
};
use git::{BuildCommitPermalinkParams, GitHostingProviderRegistry, Oid, parse_git_remote_url};
use gpui::{Action, ClipboardItem, Entity, FocusHandle, SharedString, WeakEntity, Window, actions};
use project::{GIT_COMMAND_TASK_TAG, git_store::Repository};

use task::{TaskContext, TaskVariables, VariableName};
use ui::{Color, ContextMenu, ContextMenuEntry, IconName, IconPosition, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

actions!(
    git_graph,
    [
        /// Copies the SHA of the selected commit to the clipboard.
        CopyCommitSha,
        /// Copies a tag from the selected commit to the clipboard.
        CopyCommitTag,
        /// Opens the commit view for the selected commit.
        OpenCommitView,
        /// Selects the first parent of the selected commit.
        GoToParentCommit,
        /// Selects a child of the selected commit.
        GoToChildCommit,
    ]
);

const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
pub(crate) const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitGraph,
    GitPanel,
}

pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    ref_name: Option<SharedString>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let sha = commit.sha;
    let sha_short = sha.display_short();
    let git_tasks = git_context_menu_tasks(
        git_task_context(&repository, sha, ref_name.as_deref(), cx),
        &workspace,
        cx,
    );
    let header = match &ref_name {
        Some(ref_name) => format!("Ref {ref_name}"),
        None => format!("Commit {sha_short}"),
    };
    let target = repository.clone().map(|repository| CommitTarget {
        sha,
        repository,
        workspace: workspace.clone(),
    });
    let is_head = target.as_ref().is_some_and(|target| target.is_head(cx));
    let branches = target
        .as_ref()
        .map(|target| target.branches_at_commit(cx))
        .unwrap_or_default();
    let current_branch = repository
        .as_ref()
        .and_then(WeakEntity::upgrade)
        .and_then(|repository| {
            repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string())
        });
    let permalink = repository
        .as_ref()
        .and_then(WeakEntity::upgrade)
        .and_then(|repository| {
            let remote_url = repository.read(cx).default_remote_url()?;
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
            let url = provider.build_commit_permalink(
                &parsed,
                BuildCommitPermalinkParams {
                    sha: &sha.to_string(),
                },
            );
            Some((provider.name(), url.to_string()))
        });

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        let operation = |run: fn(&CommitTarget, &mut Window, &mut App)| {
            let target = target.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(target) = &target {
                    run(target, window, cx);
                }
            }
        };
        context_menu
            .context(focus_handle)
            .header(header)
            .entry(
                "Copy Revision Number",
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry("Copy Ref Name", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .entry(
                "Create Patch…",
                None,
                operation(commit_operations::create_patch),
            )
            .entry(
                "Cherry-Pick",
                None,
                operation(commit_operations::cherry_pick),
            )
            .separator()
            .entry(
                "Checkout Revision",
                None,
                operation(commit_operations::checkout_revision),
            )
            .entry("Show Diff", Some(OpenCommitView.boxed_clone()), {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.clone() else {
                        return;
                    };
                    CommitView::open(
                        sha.to_string(),
                        repository,
                        workspace.clone(),
                        None,
                        None,
                        window,
                        cx,
                    );
                }
            })
            .when(source == CommitContextMenuSource::GitPanel, |menu| {
                menu.entry("Show in Git Log", None, move |window, cx| {
                    window.dispatch_action(
                        Box::new(crate::git_graph::OpenAtCommit {
                            sha: sha.to_string(),
                        }),
                        cx,
                    );
                })
            })
            .separator()
            .entry(
                "Reset Current Branch to Here…",
                None,
                operation(commit_operations::reset_current_branch),
            )
            .entry("Revert Commit", None, operation(commit_operations::revert))
            .item(
                ContextMenuEntry::new("Undo Commit…")
                    .disabled(!is_head)
                    .handler(operation(commit_operations::undo_commit)),
            )
            .separator()
            .entry(
                "Edit Commit Message…",
                None,
                operation(commit_operations::edit_commit_message),
            )
            .entry(
                "Fixup with Staged Changes…",
                None,
                operation(commit_operations::fixup),
            )
            .entry(
                "Drop Commit…",
                None,
                operation(commit_operations::drop_commit),
            )
            .entry(
                "Push All up to Here",
                None,
                operation(commit_operations::push_up_to),
            )
            .separator()
            .map(|mut menu| {
                if let Some(target) = &target {
                    for branch in &branches {
                        menu = commit_operations::branch_submenu(
                            menu,
                            target,
                            branch,
                            current_branch.as_deref(),
                        );
                    }
                }
                menu
            })
            .entry(
                "New Branch…",
                None,
                operation(commit_operations::new_branch),
            )
            .entry("New Tag…", None, {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.as_ref().and_then(WeakEntity::upgrade) else {
                        return;
                    };
                    workspace
                        .update(cx, |workspace, cx| {
                            create_tag_at_commit(sha, false, repository, workspace, window, cx);
                        })
                        .log_err();
                }
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = "Copy Tag";

                    match tag_names.as_slice() {
                        [] => menu.item(
                            ContextMenuEntry::new(copy_tag_label)
                                .action(CopyCommitTag.boxed_clone())
                                .disabled(true),
                        ),
                        [tag_name] => {
                            let tag_name = tag_name.clone();
                            let label = format!("{copy_tag_label}: {tag_name}");
                            menu.entry(
                                label,
                                Some(CopyCommitTag.boxed_clone()),
                                move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name.to_string(),
                                    ));
                                },
                            )
                        }
                        _ => menu.submenu(copy_tag_label, move |menu, _window, _cx| {
                            let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());

                            for tag_name in tag_names.clone() {
                                let tag_name_to_copy = tag_name.clone();
                                menu = menu.entry(tag_name, None, move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name_to_copy.to_string(),
                                    ));
                                });
                            }
                            menu
                        }),
                    }
                })
            })
            .when(source == CommitContextMenuSource::GitGraph, |menu| {
                menu.separator()
                    .action("Go to Child Commit", GoToChildCommit.boxed_clone())
                    .action("Go to Parent Commit", GoToParentCommit.boxed_clone())
            })
            .when_some(permalink.clone(), |menu, (provider, url)| {
                menu.separator().item(
                    ContextMenuEntry::new(format!("View on {provider}"))
                        .icon(IconName::ArrowUpRight)
                        .icon_color(Color::Muted)
                        .icon_position(IconPosition::End)
                        .handler(move |_window, cx| cx.open_url(&url)),
                )
            })
            .map(|mut menu| {
                menu = menu.separator().header("Custom Commands");

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new("Learn More")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .icon_position(IconPosition::End)
                            .handler(|_window, cx| {
                                let docs_url =
                                    release_channel::docs_url(CUSTOM_GIT_COMMANDS_DOCS_SLUG, cx);
                                cx.open_url(&docs_url);
                            }),
                    );
                }

                for (task_source_kind, resolved_task) in git_tasks {
                    let label = resolved_task.display_label().to_string();
                    let workspace = workspace.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.schedule_resolved_task(
                                    task_source_kind.clone(),
                                    resolved_task.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    });
                }

                menu
            })
    })
}

pub(crate) fn git_task_context(
    repository: &Option<WeakEntity<Repository>>,
    commit_sha: git::Oid,
    ref_name: Option<&str>,
    cx: &App,
) -> Option<TaskContext> {
    let repository_path = repository
        .as_ref()?
        .upgrade()?
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string);
    let mut task_variables = TaskVariables::from_iter([
        (VariableName::GitSha, commit_sha.to_string()),
        (VariableName::GitShaShort, commit_sha.display_short()),
        (
            VariableName::GitRepositoryPath,
            repository_path.to_string_lossy().into_owned(),
        ),
    ]);

    if let Some(repository_name) = repository_name {
        task_variables.insert(VariableName::GitRepositoryName, repository_name);
    }
    if let Some(ref_name) = ref_name {
        task_variables.insert(VariableName::GitRef, ref_name.to_string());
    }

    Some(TaskContext {
        cwd: Some(repository_path),
        task_variables,
        ..TaskContext::default()
    })
}

pub(crate) fn git_context_menu_tasks(
    task_context: Option<TaskContext>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
    let Some(task_context) = task_context else {
        return Vec::new();
    };
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let project = workspace.read(cx).project().clone();
    let task_inventory = project.read_with(cx, |project, cx| {
        project.task_store().read(cx).task_inventory().cloned()
    });
    let Some(task_inventory) = task_inventory else {
        return Vec::new();
    };

    task_inventory
        .read(cx)
        .resolve_global_tasks_with_tag(GIT_COMMAND_TASK_TAG, &task_context)
}

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::HashMap;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer,
    SplittableEditor, hover_markdown_style, multibuffer_context_lines,
};
use futures_lite::future::yield_now;
use git::repository::{CommitDetails, RepoPath};
use git::status::{FileStatus, StatusCode, TrackedStatus};
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, ParsedGitRemote,
    parse_git_remote_url,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    PromptLevel, Render, ScrollHandle, StatefulInteractiveElement as _, Styled, Task, WeakEntity,
    Window, actions,
};
use language::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, OffsetRangeExt as _,
    ReplicaId, Rope, TextBuffer,
};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::PathKey;
use project::{
    Project, ProjectPath, WorktreeId,
    git_store::{CommitDiff, Repository, UnshallowState},
};
use settings::{DiffViewStyle, Settings};
use std::{
    any::{Any, TypeId},
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
};
use theme::ActiveTheme;
use ui::{ContextMenu, DiffStat, Disclosure, Divider, Tooltip, WithScrollbar, prelude::*};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath, truncate_and_trailoff};
use workspace::item::TabTooltipContent;
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace, item::ItemEvent, notifications::NotifyTaskExt, pane::SaveIntent,
    searchable::SearchableItemHandle,
};

use crate::commit_tooltip::CommitAvatar;
use crate::git_panel::GitPanel;

actions!(
    git,
    [
        ApplyCurrentStash,
        PopCurrentStash,
        DropCurrentStash,
        OpenFileAtHead,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _window, _cx| {
        if let Some(project) = editor.project().map(Entity::downgrade) {
            editor.register_addon(HistoricalNavigation { project });
        }
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ApplyCurrentStash, window, cx| {
            CommitView::apply_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DropCurrentStash, window, cx| {
            CommitView::remove_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &PopCurrentStash, window, cx| {
            CommitView::pop_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenFileAtHead, window, cx| {
            let target = workspace
                .active_item_as::<Editor>(cx)
                .and_then(|editor| historical_working_copy(editor.read(cx), cx));
            if let Some(path) = target {
                workspace
                    .open_path_preview(path, None, false, false, true, window, cx)
                    .detach_and_log_err(cx);
            }
        });
    })
    .detach();
}

pub struct CommitView {
    commit: CommitDetails,
    editor: Entity<SplittableEditor>,
    message: Entity<Markdown>,
    message_expanded: bool,
    message_scroll_handle: ScrollHandle,
    stash: Option<usize>,
    multibuffer: Entity<MultiBuffer>,
    repository: Entity<Repository>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    remote: Option<GitRemote>,
    is_shallow_boundary: bool,
    file_filter: Option<RepoPath>,
    _load_diff_task: Task<Result<()>>,
}

pub(crate) struct GitBlob {
    pub(crate) path: RepoPath,
    pub(crate) repository: Option<WeakEntity<Repository>>,
    pub(crate) worktree_id: WorktreeId,
    pub(crate) is_deleted: bool,
    pub(crate) is_binary: bool,
    pub(crate) display_name: String,
}

pub(crate) fn worktree_id_for_repo_path(
    repository: &Repository,
    project: &Project,
    path: &RepoPath,
    cx: &App,
) -> Option<WorktreeId> {
    repository
        .repo_path_to_project_path(path, cx)
        .map(|project_path| project_path.worktree_id)
        .or_else(|| {
            let (worktree, _) = project.find_worktree(&repository.work_directory_abs_path, cx)?;
            Some(worktree.read(cx).id())
        })
        .or_else(|| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        })
}

struct CommitDiffAddon {
    file_statuses: HashMap<language::BufferId, FileStatus>,
    commit_view: WeakEntity<CommitView>,
}

impl Addon for CommitDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        _cx: &App,
    ) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }

    fn extend_buffer_header_context_menu(
        &self,
        menu: ContextMenu,
        buffer: &language::BufferSnapshot,
        _window: &mut Window,
        cx: &mut App,
    ) -> ContextMenu {
        let file_to_open = buffer.file().and_then(|file| {
            let commit_view = self.commit_view.upgrade()?;
            let commit_view = commit_view.read(cx);
            let project_path = commit_view
                .repository
                .read(cx)
                .repo_path_to_project_path(&RepoPath::from_rel_path(file.path()), cx)?;
            let exists_at_head = commit_view
                .workspace
                .upgrade()?
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&project_path, cx)
                .is_some();
            exists_at_head.then(|| file.clone())
        });

        menu.when_some(file_to_open, |menu, file| {
            let commit_view = self.commit_view.clone();
            menu.entry(
                "Open Working Copy",
                Some(Box::new(OpenFileAtHead)),
                move |window, cx| {
                    commit_view
                        .update(cx, |view, cx| view.open_file_at_head(&file, window, cx))
                        .log_err();
                },
            )
        })
    }
}

const FILE_NAMESPACE_SORT_PREFIX: u64 = 1;

struct HistoricalNavigation {
    project: WeakEntity<Project>,
}

impl Addon for HistoricalNavigation {
    fn to_any(&self) -> &dyn Any {
        self
    }

    fn definitions(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        kind: editor::GotoDefinitionKind,
        cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<project::LocationLink>>>>> {
        let historical = buffer.read(cx).snapshot();
        let file = historical.file()?;
        let any_file: &dyn Any = file.as_ref();
        let repository = any_file
            .downcast_ref::<GitBlob>()?
            .repository
            .as_ref()?
            .upgrade()?;
        let project = self.project.upgrade()?;
        let path = working_copy_path(repository.read(cx), project.read(cx), file.as_ref(), cx);
        Some(cx.spawn(async move |cx| {
            let Some(path) = path else {
                return Ok(None);
            };
            let current = project
                .update(cx, |project, cx| project.open_buffer(path, cx))
                .await?;
            let current_snapshot = current.read_with(cx, |buffer, _| buffer.snapshot());
            let diff = cx.new(|cx| BufferDiff::new(&current_snapshot.text, None, None, cx));
            diff.update(cx, |diff, cx| {
                diff.set_base_text(
                    Some(Arc::from(historical.text())),
                    current_snapshot.text.clone(),
                    cx,
                )
            })
            .await;
            let definitions = cx.update(|cx| {
                if &current.read(cx).version() != current_snapshot.version() {
                    return None;
                }
                let source_point = language::ToPoint::to_point(&position, &historical);
                let point = diff
                    .read(cx)
                    .snapshot(cx)
                    .base_text_point_to_buffer_point(source_point, &current_snapshot.text);
                let source_line = historical
                    .text_for_range(
                        language::Point::new(source_point.row, 0)
                            ..language::Point::new(
                                source_point.row,
                                historical.line_len(source_point.row),
                            ),
                    )
                    .collect::<String>();
                let current_line = current_snapshot
                    .text_for_range(
                        language::Point::new(point.row, 0)
                            ..language::Point::new(point.row, current_snapshot.line_len(point.row)),
                    )
                    .collect::<String>();
                // Historical buffers must never replace the server's current document.
                // Only query the working copy when the diff maps to the same code.
                if source_line != current_line {
                    return None;
                }
                editor::SemanticsProvider::definitions(
                    &project.downgrade(),
                    &current,
                    current_snapshot.anchor_before(point),
                    kind,
                    cx,
                )
            });
            let Some(definitions) = definitions else {
                return Ok(None);
            };
            let mut links = definitions.await?.unwrap_or_default();
            for link in &mut links {
                link.origin = None;
            }
            Ok(Some(links))
        }))
    }
}

fn working_copy_path(
    repository: &Repository,
    project: &Project,
    file: &dyn File,
    cx: &App,
) -> Option<ProjectPath> {
    let path = repository.repo_path_to_project_path(&RepoPath::from_rel_path(file.path()), cx)?;
    project.entry_for_path(&path, cx)?.is_file().then_some(path)
}

fn historical_working_copy(editor: &Editor, cx: &App) -> Option<ProjectPath> {
    let buffer = editor.buffer().read(cx).as_singleton()?.read(cx);
    let file = buffer.file()?;
    if !matches!(file.disk_state(), DiskState::Historic { .. }) {
        return None;
    }
    let any_file: &dyn Any = file.as_ref();
    let repository = any_file
        .downcast_ref::<GitBlob>()?
        .repository
        .as_ref()?
        .upgrade()?;
    // The editor's project, not its workspace's: the OpenFileAtHead action runs while the
    // workspace is being updated, and reading it then would panic.
    working_copy_path(
        repository.read(cx),
        editor.project()?.read(cx),
        file.as_ref(),
        cx,
    )
}

impl CommitView {
    pub fn open(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_with_options(
            commit_sha,
            repo,
            workspace,
            stash,
            file_filter,
            false,
            window,
            cx,
        )
    }

    pub(crate) fn commit_sha(&self) -> &SharedString {
        &self.commit.sha
    }

    pub(crate) fn repository(&self) -> &Entity<Repository> {
        &self.repository
    }

    fn open_with_options(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        ignore_shallow_boundary: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let commit_diff = repo
            .update(cx, |repo, cx| {
                repo.load_commit_diff(commit_sha.clone(), ignore_shallow_boundary, cx)
            })
            .ok();
        let commit_details = repo
            .update(cx, |repo, cx| repo.show_commit(commit_sha.clone(), cx))
            .ok();

        window
            .spawn(cx, async move |cx| {
                let commit_diff = commit_diff?;
                let commit_details = commit_details?;
                let (commit_diff, commit_details) = futures::join!(commit_diff, commit_details);
                let mut commit_diff = commit_diff.log_err()?;
                let commit_details = commit_details.log_err()?;

                // Filter to specific file if requested
                if let Some(ref filter_path) = file_filter {
                    commit_diff.files.retain(|f| &f.path == filter_path);
                }

                let repo = repo.upgrade()?;

                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let project = workspace.project();
                        let workspace_entity = cx.entity();
                        let workspace_handle = cx.weak_entity();
                        let commit_view = cx.new(|cx| {
                            CommitView::new(
                                commit_details,
                                commit_diff,
                                repo,
                                project.clone(),
                                workspace_entity,
                                workspace_handle,
                                stash,
                                file_filter,
                                window,
                                cx,
                            )
                        });

                        let pane = workspace.active_pane();
                        pane.update(cx, |pane, cx| {
                            let ix = pane.items().position(|item| {
                                let commit_view = item.downcast::<CommitView>();
                                commit_view
                                    .is_some_and(|view| view.read(cx).commit.sha == commit_sha)
                            });
                            if let Some(ix) = ix {
                                let existing = pane
                                    .items()
                                    .filter_map(|item| item.downcast::<CommitView>())
                                    .find(|view| view.read(cx).commit.sha == commit_sha)
                                    .unwrap();

                                pane.remove_item(existing.item_id(), false, false, window, cx);
                                pane.add_item(
                                    Box::new(commit_view),
                                    true,
                                    true,
                                    Some(ix),
                                    window,
                                    cx,
                                );
                            } else {
                                pane.add_item(Box::new(commit_view), true, true, None, window, cx);
                            }
                        })
                    })
                    .log_err()
            })
            .detach();
    }

    fn new(
        commit: CommitDetails,
        commit_diff: CommitDiff,
        repository: Entity<Repository>,
        project: Entity<Project>,
        workspace_entity: Entity<Workspace>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let is_shallow_boundary = commit_diff.is_shallow_boundary;
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let message = cx.new(|cx| {
            Markdown::new(
                commit.message.clone(),
                Some(language_registry.clone()),
                None,
                cx,
            )
        });

        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace_entity.clone(),
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);

            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
                editor.set_show_diff_review_button(true, cx);
            });

            editor
        });
        let commit_sha = Arc::<str>::from(commit.sha.as_ref());

        let repository_clone = repository.clone();
        let project_clone = project.clone();

        let load_diff_task = cx.spawn_in(window, async move |this, cx| {
            let mut binary_buffer_ids: HashSet<language::BufferId> = HashSet::default();
            let mut file_statuses: HashMap<language::BufferId, FileStatus> = HashMap::default();

            for file in commit_diff.files {
                let is_created = file.old_text.is_none();
                let is_deleted = file.new_text.is_none();
                let raw_new_text = file.new_text.unwrap_or_default();
                let raw_old_text = file.old_text;

                let is_binary = file.is_binary;

                let new_text = if is_binary {
                    "(binary file not shown)".to_string()
                } else {
                    raw_new_text
                };
                let old_text = if is_binary { None } else { raw_old_text };
                let worktree_id = repository_clone
                    .read_with(cx, |repository, cx| {
                        worktree_id_for_repo_path(
                            repository,
                            project_clone.read(cx),
                            &file.path,
                            cx,
                        )
                    })
                    .context("project has no worktrees")?;
                let short_sha = commit_sha
                    .get(0..git::SHORT_SHA_LENGTH)
                    .unwrap_or(&commit_sha);
                let file_name = file
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| file.path.display(PathStyle::local()).to_string());
                let display_name = format!("{short_sha} - {file_name}");

                let file = Arc::new(GitBlob {
                    path: file.path.clone(),
                    repository: Some(repository_clone.downgrade()),
                    is_deleted,
                    is_binary,
                    worktree_id,
                    display_name,
                }) as Arc<dyn language::File>;

                let buffer = build_buffer(new_text, file, &language_registry, cx).await?;
                let buffer_id = cx.update(|_, cx| buffer.read(cx).remote_id())?;

                let status_code = if is_created {
                    StatusCode::Added
                } else if is_deleted {
                    StatusCode::Deleted
                } else {
                    StatusCode::Modified
                };
                file_statuses.insert(
                    buffer_id,
                    FileStatus::Tracked(TrackedStatus {
                        index_status: status_code,
                        worktree_status: StatusCode::Unmodified,
                    }),
                );

                if is_binary {
                    binary_buffer_ids.insert(buffer_id);
                }

                let buffer_diff = if is_binary {
                    cx.update(|_, cx| {
                        let snapshot = buffer.read(cx).snapshot();
                        cx.new(|cx| {
                            BufferDiff::new_unchanged(
                                &snapshot,
                                snapshot.language().cloned(),
                                Some(language_registry.clone()),
                                cx,
                            )
                        })
                    })?
                } else {
                    build_buffer_diff(old_text, &buffer, &language_registry, cx).await?
                };

                let (excerpt_ranges, path) = cx.update(|_, cx| {
                    let snapshot = buffer.read(cx).snapshot();
                    let path = PathKey::with_sort_prefix(
                        FILE_NAMESPACE_SORT_PREFIX,
                        snapshot.file().unwrap().path().clone(),
                    );
                    let ranges = if is_binary {
                        vec![language::Point::zero()..snapshot.max_point()]
                    } else {
                        let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
                        let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
                        if hunks.peek().is_none() {
                            vec![language::Point::zero()..snapshot.max_point()]
                        } else {
                            hunks
                                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                                .collect::<Vec<_>>()
                        }
                    };
                    (ranges, path)
                })?;

                // Batch the insertion of excerpts and yield between batches, to avoid blocking the main thread when a single file has many hunks.
                const EXCERPT_BATCH_SIZE: usize = 10;
                let total = excerpt_ranges.len();
                let mut batch_end = 0;
                while batch_end < total {
                    let is_first_batch = batch_end == 0;
                    batch_end = (batch_end + EXCERPT_BATCH_SIZE).min(total);
                    let ranges = excerpt_ranges[..batch_end].to_vec();
                    this.update_in(cx, |this, window, cx| {
                        this.editor.update(cx, |editor, cx| {
                            editor.update_excerpts_for_path(
                                path.clone(),
                                buffer.clone(),
                                ranges,
                                multibuffer_context_lines(cx),
                                buffer_diff.clone(),
                                cx,
                            );
                            if is_first_batch && editor.diff_view_style() == DiffViewStyle::Split {
                                editor.split(window, cx);
                            }
                        });
                    })?;
                    if batch_end < total {
                        yield_now().await;
                    }
                }
            }

            this.update(cx, |this, cx| {
                let commit_view = cx.weak_entity();
                this.editor.update(cx, |editor, cx| {
                    editor.rhs_editor().update(cx, |editor, _cx| {
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                });
                if !binary_buffer_ids.is_empty() {
                    this.editor.update(cx, |editor, cx| {
                        editor.rhs_editor().update(cx, |editor, cx| {
                            editor.fold_buffers(binary_buffer_ids, cx);
                        });
                    });
                }
            })?;

            anyhow::Ok(())
        });

        let snapshot = repository.read(cx).snapshot();
        let remote_url = snapshot
            .remote_upstream_url
            .as_ref()
            .or(snapshot.remote_origin_url.as_ref());

        let remote = remote_url.and_then(|url| {
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            parse_git_remote_url(provider_registry, url).map(|(host, parsed)| GitRemote {
                host,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        Self {
            commit,
            editor,
            message,
            message_expanded: false,
            message_scroll_handle: ScrollHandle::new(),
            multibuffer,
            stash,
            repository,
            project,
            workspace,
            remote,
            is_shallow_boundary,
            file_filter,
            _load_diff_task: load_diff_task,
        }
    }

    fn render_shallow_boundary_notice(&self, cx: &App) -> impl IntoElement {
        let commit_sha = self.commit.sha.to_string();
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        let stash = self.stash;
        let file_filter = self.file_filter.clone();
        let unshallow_state = self.repository.read(cx).unshallow_state();
        let can_fetch = !self.project.read(cx).is_via_collab()
            && unshallow_state != UnshallowState::Unshallowed;
        let fetch_in_flight = unshallow_state == UnshallowState::InProgress;
        v_flex()
            .flex_grow(1.)
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Label::new("This commit is at the boundary of a shallow clone.")
                    .color(Color::Muted),
            )
            .child(
                Label::new(
                    "Its parent history was not fetched, so the changes it introduced cannot be shown.",
                )
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(can_fetch, |this| {
                        let commit_sha = commit_sha.clone();
                        let repository = repository.clone();
                        let workspace = workspace.clone();
                        let file_filter = file_filter.clone();
                        this.child(
                            Button::new(
                                "fetch-unshallow",
                                if fetch_in_flight {
                                    "Fetching…"
                                } else {
                                    "Fetch Missing History"
                                },
                            )
                                .style(ButtonStyle::Filled)
                                .disabled(fetch_in_flight)
                                .tooltip(Tooltip::text(
                                    "Run `git fetch --unshallow` to download the full history, then show this commit's changes.",
                                ))
                                .on_click(move |_, window, cx| {
                                    let fetch = crate::commit_tooltip::fetch_unshallow(
                                        repository.clone(),
                                        workspace.clone(),
                                        window,
                                        cx,
                                    );
                                    let commit_sha = commit_sha.clone();
                                    let repository = repository.downgrade();
                                    let workspace = workspace.clone();
                                    let file_filter = file_filter.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            fetch.await?;
                                            cx.update(|window, cx| {
                                                Self::open_with_options(
                                                    commit_sha,
                                                    repository,
                                                    workspace,
                                                    stash,
                                                    file_filter,
                                                    false,
                                                    window,
                                                    cx,
                                                )
                                            })
                                        })
                                        .detach_and_log_err(cx);
                                }),
                        )
                    })
                    .child(
                        Button::new(
                            "load-shallow-snapshot",
                            if file_filter.is_some() {
                                "Load File Snapshot"
                            } else {
                                "Load Full Snapshot"
                            },
                        )
                            .style(ButtonStyle::Outlined)
                            .tooltip(Tooltip::text(if file_filter.is_some() {
                                "Show this file's full contents at this commit as added."
                            } else {
                                "Show every file at this commit as added. This can be slow in large repositories."
                            }))
                            .on_click(move |_, window, cx| {
                                Self::open_with_options(
                                    commit_sha.clone(),
                                    repository.downgrade(),
                                    workspace.clone(),
                                    stash,
                                    file_filter.clone(),
                                    true,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    }

    fn render_commit_avatar(
        &self,
        sha: &SharedString,
        size: impl Into<gpui::AbsoluteLength>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        CommitAvatar::new(
            sha,
            Some(self.commit.author_email.clone()),
            self.remote.as_ref(),
        )
        .size(size)
        .render(window, cx)
    }

    fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    fn open_file_at_head(
        &mut self,
        file: &Arc<dyn language::File>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project_path) = working_copy_path(
            self.repository.read(cx),
            self.project.read(cx),
            file.as_ref(),
            cx,
        ) else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error("This file does not exist in the working copy", cx);
                })
                .log_err();
            return;
        };

        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(project_path, None, false, false, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn open_file_at_head_action(
        &mut self,
        _: &OpenFileAtHead,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .editor
            .read(cx)
            .focused_editor()
            .read(cx)
            .active_buffer(cx)
            .and_then(|buffer| buffer.read(cx).file().cloned())
        else {
            return;
        };
        self.open_file_at_head(&file, window, cx);
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let commit = &self.commit;
        let author_name = commit.author_name.clone();
        let author_email = commit.author_email.clone();
        let commit_sha = commit.sha.clone();
        let commit_date = time::OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let date_string = time_format::format_localized_timestamp(
            commit_date,
            time::OffsetDateTime::now_utc(),
            local_offset,
            time_format::TimestampFormat::MediumAbsolute,
        );

        let avatar_size = rems_from_px(40_f32);
        let avatar_size_px = avatar_size.to_pixels(window.rem_size());
        let gutter_width = self.editor.update(cx, |editor, cx| {
            let editor = editor.rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let style = editor.style(cx);
                let font_id = window.text_system().resolve_font(&style.text.font());
                let font_size = style.text.font_size.to_pixels(window.rem_size());
                snapshot
                    .gutter_dimensions(font_id, font_size, style, window, cx)
                    .full_width()
            })
        });
        let avatar_min_side_padding = rems_from_px(6_f32).to_pixels(window.rem_size());
        let avatar_container_min = avatar_size_px + avatar_min_side_padding;
        let avatar_container_width = gutter_width.max(avatar_container_min);

        let clipboard_has_sha = cx
            .read_from_clipboard()
            .and_then(|entry| entry.text())
            .map_or(false, |clipboard_text| {
                clipboard_text.trim() == commit_sha.as_ref()
            });

        let (copy_icon, copy_icon_color) = if clipboard_has_sha {
            (IconName::Check, Color::Success)
        } else {
            (IconName::Copy, Color::Muted)
        };

        let has_more = self.commit.message.trim().contains('\n');
        let is_expanded = self.message_expanded;
        let expand_tooltip = if is_expanded {
            "Fold Commit Description"
        } else {
            "Expand Commit Description"
        };

        v_flex()
            .w_full()
            .py_2p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .pr_2p5()
                    .w_full()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        h_flex()
                            .child(
                                h_flex()
                                    .flex_none()
                                    .w(avatar_container_width)
                                    .justify_center()
                                    .child(self.render_commit_avatar(
                                        &commit.sha,
                                        avatar_size,
                                        window,
                                        cx,
                                    )),
                            )
                            .child(
                                v_flex()
                                    .child(h_flex().gap_1().child(Label::new(author_name)).when(
                                        has_more,
                                        |this| {
                                            this.child(
                                                Disclosure::new(
                                                    "commit-message-disclosure",
                                                    is_expanded,
                                                )
                                                .closed_icon(IconName::ExpandVertical)
                                                .opened_icon(IconName::FoldVertical)
                                                .tooltip(Tooltip::text(expand_tooltip))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.message_expanded = !this.message_expanded;
                                                    cx.notify();
                                                })),
                                            )
                                        },
                                    ))
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(
                                                Label::new(date_string)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            )
                                            .child(
                                                Label::new("•")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .alpha(0.5),
                                            )
                                            .child(
                                                Label::new(author_email)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            ),
                                    ),
                            ),
                    )
                    .when(self.stash.is_none(), |this| {
                        this.child(
                            Button::new("sha", "Commit SHA")
                                .start_icon(
                                    Icon::new(copy_icon)
                                        .size(IconSize::Small)
                                        .color(copy_icon_color),
                                )
                                .tooltip({
                                    let commit_sha = commit_sha.clone();
                                    move |_, cx| {
                                        Tooltip::with_meta(
                                            "Copy Commit SHA",
                                            None,
                                            commit_sha.clone(),
                                            cx,
                                        )
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        commit_sha.to_string(),
                                    ));
                                }),
                        )
                    }),
            )
            .children(self.render_commit_message(avatar_container_width, window, cx))
    }

    fn render_commit_message(
        &self,
        avatar_spacer: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let message = self.commit.message.trim();
        if message.is_empty() {
            return None;
        }

        let markdown_style = hover_markdown_style(window, cx);

        let is_expanded = self.message_expanded;

        let has_more = message.contains('\n');
        let collapsed = has_more && !is_expanded;
        let collapsed_height = window.line_height();
        let max_expanded_height = window.line_height() * 12.;

        Some(
            h_flex()
                .w_full()
                .pr_2p5()
                .child(h_flex().flex_none().w(avatar_spacer))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .id("commit-message")
                                .size_full()
                                .text_sm()
                                .when(collapsed, |this| this.h(collapsed_height).overflow_hidden())
                                .when(!collapsed, |this| {
                                    this.max_h(max_expanded_height)
                                        .overflow_y_scroll()
                                        .track_scroll(&self.message_scroll_handle)
                                })
                                .child(MarkdownElement::new(self.message.clone(), markdown_style)),
                        )
                        .vertical_scrollbar_for(&self.message_scroll_handle, window, cx),
                ),
        )
    }

    fn apply_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Apply",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, not applying"));
                    }
                    Ok(repo.stash_apply(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn pop_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Pop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, pop aborted"));
                    }
                    Ok(repo.stash_pop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn remove_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Drop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, drop aborted"));
                    }
                    Ok(repo.stash_drop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await??,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn stash_action<AsyncFn>(
        workspace: &mut Workspace,
        str_action: &str,
        window: &mut Window,
        cx: &mut App,
        callback: AsyncFn,
    ) where
        AsyncFn: AsyncFnOnce(
                Entity<Repository>,
                &SharedString,
                usize,
                Entity<CommitView>,
                WeakEntity<Workspace>,
                &mut AsyncWindowContext,
            ) -> anyhow::Result<()>
            + 'static,
    {
        let Some(commit_view) = workspace.active_item_as::<CommitView>(cx) else {
            return;
        };
        let Some(stash) = commit_view.read(cx).stash else {
            return;
        };
        let sha = commit_view.read(cx).commit.sha.clone();
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("{} stash@{{{}}}?", str_action, stash),
            None,
            &[str_action, "Cancel"],
            cx,
        );

        let workspace_weak = workspace.weak_handle();
        let commit_view_entity = commit_view;

        window
            .spawn(cx, async move |cx| {
                if answer.await != Ok(0) {
                    return anyhow::Ok(());
                }

                let Some(workspace) = workspace_weak.upgrade() else {
                    return Ok(());
                };

                let repo = workspace.update(cx, |workspace, cx| {
                    workspace
                        .panel::<GitPanel>(cx)
                        .and_then(|p| p.read(cx).active_repository.clone())
                });

                let Some(repo) = repo else {
                    return Ok(());
                };

                callback(repo, &sha, stash, commit_view_entity, workspace_weak, cx).await?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace.weak_handle(), window, cx);
    }

    async fn close_commit_view(
        commit_view: Entity<CommitView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> anyhow::Result<()> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                let active_pane = workspace.active_pane();
                let commit_view_id = commit_view.entity_id();
                active_pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(commit_view_id, SaveIntent::Skip, window, cx)
                })
            })?
            .await?;
        anyhow::Ok(())
    }
}

impl language::File for GitBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

pub(crate) async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<language::LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language_id) = language {
        language_registry
            .load_language(language_id)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite, cx);
        buffer.set_language_registry(language_registry.clone());
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|_, cx| buffer.read(cx).language().cloned())?;
    let buffer = cx.update(|_, cx| buffer.read(cx).snapshot())?;

    let diff =
        cx.new(|cx| BufferDiff::new(&buffer.text, language, Some(language_registry.clone()), cx));

    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            old_text.map(|old_text| Arc::from(old_text.as_str())),
            buffer.text.clone(),
            cx,
        )
    })
    .await;

    Ok(diff)
}

impl EventEmitter<EditorEvent> for CommitView {}

impl Focusable for CommitView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CommitView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitCommit).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let short_sha = self.commit.sha.get(0..7).unwrap_or(&*self.commit.sha);
        let subject = truncate_and_trailoff(self.commit.message.split('\n').next().unwrap(), 20);
        format!("{short_sha} — {subject}").into()
    }

    fn tab_tooltip_content(&self, _: &App) -> Option<TabTooltipContent> {
        let short_sha = self.commit.sha.get(0..16).unwrap_or(&*self.commit.sha);
        let subject = self.commit.message.split('\n').next().unwrap();

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let subject = subject.to_string();
            let short_sha = short_sha.to_string();

            move |_, _| {
                v_flex()
                    .child(Label::new(subject.clone()))
                    .child(
                        Label::new(short_sha.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Commit View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let file_statuses = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .addon::<CommitDiffAddon>()
            .map(|addon| addon.file_statuses.clone())
            .unwrap_or_default();
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let project = self.project.clone();
        let diff_view_style = self.editor.read(cx).diff_view_style();
        let multibuffer = self.multibuffer.clone();
        Task::ready(Some(cx.new(|cx| {
            let commit_view = cx.weak_entity();
            let editor = cx.new({
                let file_statuses = file_statuses.clone();
                let project = project.clone();
                let workspace_entity = workspace_entity.clone();
                let multibuffer = multibuffer.clone();
                move |cx| {
                    let editor = SplittableEditor::new(
                        diff_view_style,
                        multibuffer.clone(),
                        project.clone(),
                        workspace_entity.clone(),
                        window,
                        cx,
                    );
                    editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, |editor, cx| {
                        editor.set_show_bookmarks(false, cx);
                        editor.set_show_breakpoints(false, cx);
                        editor.set_show_diff_review_button(true, cx);
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                    editor
                }
            });
            let language_registry = project.read(cx).languages().clone();
            let message = cx.new(|cx| {
                Markdown::new(
                    self.commit.message.clone(),
                    Some(language_registry),
                    None,
                    cx,
                )
            });
            Self {
                editor,
                message,
                message_expanded: self.message_expanded,
                message_scroll_handle: ScrollHandle::new(),
                multibuffer: self.multibuffer.clone(),
                commit: self.commit.clone(),
                stash: self.stash,
                repository: self.repository.clone(),
                project: self.project.clone(),
                workspace: self.workspace.clone(),
                remote: self.remote.clone(),
                is_shallow_boundary: self.is_shallow_boundary,
                file_filter: self.file_filter.clone(),
                _load_diff_task: Task::ready(Ok(())),
            }
        })))
    }
}

impl Render for CommitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_stash = self.stash.is_some();

        v_flex()
            .key_context(if is_stash { "StashDiff" } else { "CommitDiff" })
            .on_action(cx.listener(Self::open_file_at_head_action))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(window, cx))
            .when(
                !self.editor.read(cx).rhs_editor().read(cx).is_empty(cx),
                |this| this.child(div().flex_grow(1.).child(self.editor.clone())),
            )
            .when(self.is_shallow_boundary, |this| {
                this.child(self.render_shallow_boundary_notice(cx))
            })
    }
}

pub struct CommitViewToolbar {
    commit_view: Option<WeakEntity<CommitView>>,
    historical_editor: Option<WeakEntity<Editor>>,
}

impl CommitViewToolbar {
    pub fn new() -> Self {
        Self {
            commit_view: None,
            historical_editor: None,
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for CommitViewToolbar {}

impl Render for CommitViewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(editor) = self
            .historical_editor
            .as_ref()
            .and_then(|editor| editor.upgrade())
        {
            let available = historical_working_copy(editor.read(cx), cx).is_some();
            return h_flex()
                .debug_selector(|| "open-working-copy".to_string())
                .child(
                    Button::new("open-working-copy", "Open Working Copy")
                        .disabled(!available)
                        .tooltip(Tooltip::text(if available {
                            "Open the current editable file in the project"
                        } else {
                            "This file does not exist in the working copy"
                        }))
                        .on_click(move |_, window, cx| {
                            let target = historical_working_copy(editor.read(cx), cx);
                            let workspace = editor.read(cx).workspace();
                            if let Some((path, workspace)) = target.zip(workspace) {
                                workspace.update(cx, |workspace, cx| {
                                    workspace
                                        .open_path_preview(
                                            path, None, false, false, true, window, cx,
                                        )
                                        .detach_and_log_err(cx);
                                });
                            }
                        }),
                );
        }
        let Some(commit_view) = self.commit_view.as_ref().and_then(|w| w.upgrade()) else {
            return h_flex();
        };

        let commit_view_ref = commit_view.read(cx);
        let is_stash = commit_view_ref.stash.is_some();

        let (additions, deletions) = commit_view_ref.calculate_changed_lines(cx);

        let commit_sha = commit_view_ref.commit.sha.clone();

        let remote_info = commit_view_ref.remote.as_ref().map(|remote| {
            let provider = remote.host.name();
            let parsed_remote = ParsedGitRemote {
                owner: remote.owner.as_ref().into(),
                repo: remote.repo.as_ref().into(),
            };
            let params = BuildCommitPermalinkParams { sha: &commit_sha };
            let url = remote
                .host
                .build_commit_permalink(&parsed_remote, params)
                .to_string();
            (provider, url)
        });

        let sha_for_graph = commit_sha.to_string();

        h_flex()
            .gap_1()
            .child(
                Button::new("open-working-copy", "Open Working Copy")
                    .tooltip(Tooltip::text(
                        "Open the selected file's current editable working copy",
                    ))
                    .on_click(move |_, window, cx| {
                        commit_view.update(cx, |view, cx| {
                            view.open_file_at_head_action(&OpenFileAtHead, window, cx);
                        });
                    }),
            )
            .when(additions > 0 || deletions > 0, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(DiffStat::new(
                            "toolbar-diff-stat",
                            additions as usize,
                            deletions as usize,
                        ))
                        .child(Divider::vertical()),
                )
            })
            .child(
                IconButton::new("buffer-search", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action(
                            "Buffer Search",
                            &zed_actions::buffer_search::Deploy::find(),
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::buffer_search::Deploy::find()),
                            cx,
                        );
                    }),
            )
            .when(!is_stash, |this| {
                this.child(
                    IconButton::new("show-in-git-graph", IconName::GitGraph)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Show in Git Graph"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::git_graph::OpenAtCommit {
                                    sha: sha_for_graph.clone(),
                                }),
                                cx,
                            );
                        }),
                )
                .children(remote_info.map(|(provider_name, url)| {
                    let icon = ui::git_hosting_provider_icon(provider_name.as_str());

                    IconButton::new("view_on_provider", icon)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(format!("View on {}", provider_name)))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
            })
    }
}

impl ToolbarItemView for CommitViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.historical_editor = None;
        if let Some(entity) = active_pane_item.and_then(|i| i.act_as::<CommitView>(cx)) {
            self.commit_view = Some(entity.downgrade());
            return ToolbarItemLocation::PrimaryRight;
        }
        self.commit_view = None;
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            let is_historical = editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| buffer.read(cx).file())
                .is_some_and(|file| matches!(file.disk_state(), DiskState::Historic { .. }));
            if is_historical {
                self.historical_editor = Some(editor.downgrade());
                return ToolbarItemLocation::PrimaryRight;
            }
        }
        ToolbarItemLocation::Hidden
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

fn stash_matches_index(sha: &str, stash_index: usize, repo: &Repository) -> bool {
    repo.stash_entries
        .entries
        .get(stash_index)
        .map(|entry| entry.oid.to_string() == sha)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EmptyView, TestAppContext};
    use indoc::indoc;
    use language::{Language, LanguageConfig, LanguageMatcher, markdown_lang};
    use settings::SettingsStore;

    #[gpui::test]
    async fn test_navigation_historical_go_symbols_in_diff_and_file(cx: &mut TestAppContext) {
        use editor::test::editor_lsp_test_context::EditorLspTestContext;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let language = Language::new(
            LanguageConfig {
                name: "Go".into(),
                matcher: Arc::new(LanguageMatcher {
                    path_suffixes: vec!["go".into()],
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        );
        let mut cx = EditorLspTestContext::new(
            language,
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.update(|_, cx| super::init(cx));
        cx.set_state("// current only\npackage main\nfunc main() { ˇwork() }\nfunc work() {}\n");
        let current_editor = cx.editor.clone();
        let (project, repository) = cx.workspace.read_with(&**cx, |workspace, cx| {
            let project = workspace.project().clone();
            let repository = project
                .read(cx)
                .active_repository(cx)
                .expect("fixture repository");
            (project, repository)
        });
        let target = cx
            .lsp_range("// current only\npackage main\nfunc main() { work() }\nfunc «work»() {}\n");
        let requests = Arc::new(AtomicUsize::new(0));
        let _handler = cx.set_request_handler::<lsp::request::GotoDefinition, _, _>({
            let requests = requests.clone();
            move |uri, params, _| {
                assert_eq!(params.text_document_position_params.position.line, 2);
                requests.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(Some(lsp::GotoDefinitionResponse::Array(vec![
                        lsp::Location { uri, range: target },
                    ])))
                }
            }
        });
        let historical_text = "package main\nfunc main() { work() }\nfunc work() {}\n";
        for multibuffer in [false, true] {
            let path = RepoPath::new("dir/file.go").unwrap();
            let worktree_id = repository.read_with(&**cx, |repository, cx| {
                worktree_id_for_repo_path(repository, project.read(cx), &path, cx).unwrap()
            });
            let blob = Arc::new(GitBlob {
                path,
                repository: Some(repository.downgrade()),
                worktree_id,
                is_deleted: false,
                is_binary: false,
                display_name: "abc1234 - file.go".into(),
            }) as Arc<dyn File>;
            let languages = project.read_with(&**cx, |project, _| project.languages().clone());
            let mut async_cx = cx.update(|window, cx| window.to_async(cx));
            let buffer = build_buffer(historical_text.into(), blob, &languages, &mut async_cx)
                .await
                .unwrap();
            let historical_editor = cx.update(|window, cx| {
                let multi_buffer = cx.new(|cx| {
                    if multibuffer {
                        let mut multi_buffer = MultiBuffer::new(Capability::ReadOnly);
                        multi_buffer.set_excerpts_for_path(
                            PathKey::sorted(0),
                            buffer.clone(),
                            [language::Point::zero()..buffer.read(cx).max_point()],
                            0,
                            cx,
                        );
                        multi_buffer
                    } else {
                        MultiBuffer::singleton(buffer.clone(), cx)
                    }
                });
                cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx);
                    editor.set_read_only(true);
                    editor
                })
            });
            cx.workspace
                .clone()
                .update_in(&mut **cx, |workspace, window, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(historical_editor.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
            cx.editor = historical_editor.clone();
            cx.run_until_parked();
            let count = requests.load(Ordering::SeqCst);
            let position =
                cx.pixel_position("package main\nfunc main() { woˇrk() }\nfunc work() {}\n");
            let modifiers = cx.read(gpui::Modifiers::secondary_key);
            cx.simulate_mouse_move(position, None, modifiers);
            cx.run_until_parked();
            assert_eq!(
                requests.load(Ordering::SeqCst),
                count + 1,
                "historical symbol must query the working-copy server"
            );
            cx.assert_editor_text_highlights(
                editor::display_map::HighlightKey::HoveredLinkState,
                "package main\nfunc main() { «work»() }\nfunc work() {}\n",
            );
            cx.simulate_click(position, modifiers);
            cx.run_until_parked();
            assert_eq!(
                cx.workspace.read_with(&**cx, |workspace, cx| workspace
                    .active_item_as::<Editor>(cx)),
                Some(current_editor.clone())
            );
            cx.editor = current_editor.clone();
            cx.assert_editor_state(
                "// current only\npackage main\nfunc main() { work() }\nfunc «workˇ»() {}\n",
            );
            historical_editor.read_with(&**cx, |editor, cx| assert!(editor.read_only(cx)));
            buffer.read_with(&**cx, |buffer, _| {
                assert_eq!(buffer.text(), historical_text)
            });
            cx.workspace
                .clone()
                .update_in(&mut **cx, |workspace, window, cx| {
                    workspace.activate_item(&historical_editor, true, true, window, cx);
                });
            cx.editor = historical_editor.clone();
            cx.simulate_modifiers_change(gpui::Modifiers::none());
            cx.dispatch_action(editor::actions::GoToDefinition::default());
            cx.run_until_parked();
            assert_eq!(
                cx.workspace.read_with(&**cx, |workspace, cx| workspace
                    .active_item_as::<Editor>(cx)),
                Some(current_editor.clone())
            );

            cx.editor = current_editor.clone();
            cx.set_state(
                "// current only\npackage main\nfunc main() { ˇother() }\nfunc work() {}\n",
            );
            cx.workspace
                .clone()
                .update_in(&mut **cx, |workspace, window, cx| {
                    workspace.activate_item(&historical_editor, true, true, window, cx);
                });
            cx.editor = historical_editor;
            cx.run_until_parked();
            let count = requests.load(Ordering::SeqCst);
            let position =
                cx.pixel_position("package main\nfunc main() { woˇrk() }\nfunc work() {}\n");
            cx.simulate_mouse_move(position, None, modifiers);
            cx.run_until_parked();
            assert_eq!(
                requests.load(Ordering::SeqCst),
                count,
                "changed code must not query an unrelated current symbol"
            );
            cx.assert_editor_text_highlights(
                editor::display_map::HighlightKey::HoveredLinkState,
                historical_text,
            );
            cx.editor = current_editor.clone();
            cx.set_state(
                "// current only\npackage main\nfunc main() { ˇwork() }\nfunc work() {}\n",
            );
        }
    }

    #[gpui::test]
    async fn test_navigation_open_historical_working_copy_in_nested_repository(
        cx: &mut TestAppContext,
    ) {
        let app_state = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state, cx);
            super::init(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            std::path::Path::new("/workspace"),
            serde_json::json!({
                "nested": {".git": {}, "src": {"main.go": "package current\n"}},
                "src": {"main.go": "wrong file\n"},
            }),
        )
        .await;
        let project = Project::test(fs, [std::path::Path::new("/workspace")], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let path = RepoPath::new("src/main.go").unwrap();
        let worktree_id = repository.read_with(cx, |repository, cx| {
            worktree_id_for_repo_path(repository, project.read(cx), &path, cx).unwrap()
        });
        let blob = Arc::new(GitBlob {
            path,
            repository: Some(repository.downgrade()),
            worktree_id,
            is_deleted: false,
            is_binary: false,
            display_name: "abc1234 - main.go".into(),
        }) as Arc<dyn File>;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        let mut async_cx = cx.update(|window, cx| window.to_async(cx));
        let buffer = build_buffer(
            "package historical\n".into(),
            blob,
            &language_registry,
            &mut async_cx,
        )
        .await
        .unwrap();
        let historical_editor = workspace.update_in(cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.toolbar().update(cx, |toolbar, cx| {
                    let controls = cx.new(|_| CommitViewToolbar::new());
                    toolbar.add_item(controls, window, cx);
                });
            });
            let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
            let editor = cx.new(|cx| {
                let mut editor =
                    Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx);
                editor.set_read_only(true);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let button = cx
            .debug_bounds("open-working-copy")
            .expect("working copy button must be visible");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("working copy editor");
            assert_ne!(editor, historical_editor);
            assert!(!editor.read(cx).read_only(cx));
            assert_eq!(
                editor.read(cx).buffer().read(cx).snapshot(cx).text(),
                "package current\n"
            );
            assert!(historical_editor.read(cx).read_only(cx));
            assert_eq!(buffer.read(cx).text(), "package historical\n");
        });

        // The action takes the same route while the workspace handles it.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&historical_editor, true, true, window, cx);
        });
        cx.dispatch_action(OpenFileAtHead);
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("working copy editor");
            assert_ne!(editor, historical_editor);
            assert!(!editor.read(cx).read_only(cx));
        });
    }

    #[gpui::test]
    async fn test_build_buffer_resolves_injected_languages(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.add(markdown_lang());
        language_registry.add(Arc::new(markdown_inline_lang()));

        let window = cx.add_window(|_, _| EmptyView);
        let mut async_cx = window
            .update(cx, |_, window, cx| window.to_async(cx))
            .expect("window should be open");

        let text = indoc! {"
            # Title

            Some *emphasized* text.
        "}
        .to_string();
        let blob = Arc::new(GitBlob {
            path: RepoPath::new("notes.md").unwrap(),
            repository: None,
            worktree_id: WorktreeId::from_usize(0),
            is_deleted: false,
            is_binary: false,
            display_name: "abc1234 - notes.md".into(),
        }) as Arc<dyn File>;

        let buffer = build_buffer(text, blob, &language_registry, &mut async_cx)
            .await
            .expect("buffer should build");

        cx.run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            let language = buffer.language().expect("buffer should have a language");
            assert_eq!(language.name().as_ref(), "Markdown");

            let layers = buffer
                .snapshot()
                .syntax_layers()
                .map(|layer| layer.language.name().to_string())
                .collect::<Vec<_>>();
            assert!(
                layers.iter().any(|name| name == "Markdown-Inline"),
                "emphasis, links, and fenced code blocks are highlighted by the injected \
                 Markdown-Inline grammar, but the buffer parsed with layers {layers:?}",
            );
        });
    }

    #[gpui::test]
    fn test_git_blob_language_detection_ignores_display_name(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store
                .set_user_settings(r#"{"file_types":{"JSONC":["tsconfig*.json"]}}"#, cx)
                .expect("valid file type settings");
            cx.set_global(store);
        });

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.register_test_language(LanguageConfig {
            name: "JSON".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["json".into(), "flake.lock".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        language_registry.register_test_language(LanguageConfig {
            name: "JSONC".into(),
            ..LanguageConfig::default()
        });

        let languages = cx.update(|cx| {
            [
                ("flake.lock", "abc1234 - flake.lock"),
                ("flake.lock", "flake.lock @ abc1234"),
                ("nix/flake.lock", "abc1234 - flake.lock"),
                ("nix/flake.lock", "flake.lock @ abc1234"),
                ("web/tsconfig.json", "abc1234 - tsconfig.json"),
                ("web/tsconfig.json", "tsconfig.json @ abc1234"),
            ]
            .map(|(path, display_name)| {
                let blob = Arc::new(GitBlob {
                    path: RepoPath::new(path).expect("valid repository path"),
                    repository: None,
                    worktree_id: WorktreeId::from_usize(0),
                    is_deleted: false,
                    is_binary: false,
                    display_name: display_name.to_string(),
                }) as Arc<dyn File>;
                assert_eq!(blob.file_system_abs_path(cx), None);
                language_registry
                    .language_for_file(&blob, None, cx)
                    .and_then(|id| language_registry.language_name_for_id(id))
                    .map(|name| name.to_string())
            })
        });

        assert_eq!(
            languages.each_ref().map(|name| name.as_deref()),
            [
                Some("JSON"),
                Some("JSON"),
                Some("JSON"),
                Some("JSON"),
                Some("JSONC"),
                Some("JSONC"),
            ]
        );
    }

    fn markdown_inline_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Markdown-Inline".into(),
                hidden: true,
                ..LanguageConfig::default()
            },
            Some(tree_sitter_md::INLINE_LANGUAGE.into()),
        )
        .with_highlights_query("(emphasis) @emphasis")
        .unwrap()
    }
}

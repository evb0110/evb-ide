//! Pull request review threads in the editor, like JetBrains' GitHub review mode: each thread
//! shows under its line, with replies, resolving and new line comments.

use crate::pull_requests::{RefreshPullRequestStatus, github_client};
use collections::{HashMap, HashSet};
use editor::{
    Editor,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use evb_github::{
    GitHubError, NewReviewComment, PullRequestReview, RepositoryLocator, ReviewThread,
    ReviewVerdict,
};
use gpui::{
    Action as _, AnyElement, App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Global, SharedString, Subscription, WeakEntity, Window,
};
use language::Point;
use menu::{Cancel, Confirm};
use notifications::status_toast::StatusToast;
use std::{
    future::Future,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{KeyBinding, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, OpenOptions, Workspace};

/// Wide enough for most comments, narrow enough to read.
const CARD_WIDTH_IN_REMS: f32 = 44.;
/// A rough count of characters a card line holds, to size blocks before they render.
const CARD_LINE_CHARACTERS: usize = 90;

/// The review of a repository's current branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchReview {
    pub repository: RepositoryLocator,
    pub url: String,
    pub review: PullRequestReview,
}

#[derive(Default)]
struct ReviewCache(HashMap<Arc<Path>, Arc<BranchReview>>);

impl Global for ReviewCache {}

/// Records the review of the branch checked out in `work_directory`; editors update when it
/// changes.
pub(crate) fn set_branch_review(
    work_directory: Arc<Path>,
    review: Option<BranchReview>,
    cx: &mut App,
) {
    let current = cx
        .try_global::<ReviewCache>()
        .and_then(|cache| cache.0.get(&work_directory).cloned());
    if current.as_deref() == review.as_ref() {
        return;
    }
    cx.default_global::<ReviewCache>();
    cx.update_global::<ReviewCache, _>(|cache, _| match review {
        Some(review) => {
            cache.0.insert(work_directory, Arc::new(review));
        }
        None => {
            cache.0.remove(&work_directory);
        }
    });
}

pub(crate) fn branch_review(work_directory: &Path, cx: &App) -> Option<Arc<BranchReview>> {
    cx.try_global::<ReviewCache>()?
        .0
        .get(work_directory)
        .cloned()
}

/// The review covering a file, and the file's path in the repository.
fn review_for_file(path: &Path, cx: &App) -> Option<(Arc<Path>, String, Arc<BranchReview>)> {
    let cache = cx.try_global::<ReviewCache>()?;
    cache.0.iter().find_map(|(work_directory, review)| {
        let relative = path.strip_prefix(work_directory).ok()?;
        let relative = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        Some((work_directory.clone(), relative, review.clone()))
    })
}

fn editor_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Some(file.as_local()?.abs_path(cx))
}

struct ReviewAddon {
    blocks: Vec<CustomBlockId>,
    /// What the blocks show, so unrelated cache updates leave them alone.
    shown: Vec<ReviewThread>,
    expanded: HashSet<String>,
    comment_action: Option<Subscription>,
    _cache_observer: Subscription,
}

impl editor::Addon for ReviewAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

pub(crate) fn register_editor(editor: &mut Editor, cx: &mut Context<Editor>) {
    if !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton() {
        return;
    }
    let observer = cx.observe_global::<ReviewCache>(|editor, cx| refresh_blocks(editor, false, cx));
    editor.register_addon(ReviewAddon {
        blocks: Vec::new(),
        shown: Vec::new(),
        expanded: HashSet::default(),
        comment_action: None,
        _cache_observer: observer,
    });
    refresh_blocks(editor, false, cx);
}

fn refresh_blocks(editor: &mut Editor, force: bool, cx: &mut Context<Editor>) {
    let found = editor_path(editor, cx).and_then(|path| review_for_file(&path, cx));
    let threads: Vec<ReviewThread> = found
        .as_ref()
        .map(|(_, relative, review)| {
            review
                .review
                .threads
                .iter()
                .filter(|thread| &thread.path == relative && thread.line.is_some())
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    let wants_action = found.is_some();
    let has_action = editor
        .addon::<ReviewAddon>()
        .is_some_and(|addon| addon.comment_action.is_some());
    if wants_action != has_action {
        let action = wants_action.then(|| {
            let editor_handle = cx.weak_entity();
            editor.register_action(move |action: &git::AddPullRequestComment, window, cx| {
                add_comment(editor_handle.clone(), action.row, window, cx);
            })
        });
        if let Some(addon) = editor.addon_mut::<ReviewAddon>() {
            addon.comment_action = action;
        }
    }

    let Some(addon) = editor.addon::<ReviewAddon>() else {
        return;
    };
    if !force && addon.shown == threads {
        return;
    }
    let old_blocks = addon.blocks.iter().copied().collect::<HashSet<_>>();
    let expanded = addon.expanded.clone();
    editor.remove_blocks(old_blocks, None, cx);

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let max_row = snapshot.max_point().row;
    let editor_handle = cx.weak_entity();
    let mut blocks = Vec::new();
    if let Some((_, _, review)) = &found {
        for thread in &threads {
            let row = thread.line.unwrap_or(1).saturating_sub(1).min(max_row);
            // Below the line: the anchor's row is the one the thread is about.
            let anchor = snapshot.anchor_after(Point::new(row, 0));
            let is_expanded = !thread.is_resolved || expanded.contains(&thread.id);
            let thread = thread.clone();
            let review = review.clone();
            let editor_handle = editor_handle.clone();
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(thread_height(&thread, is_expanded)),
                style: BlockStyle::Sticky,
                render: Arc::new(move |cx| {
                    render_thread(&thread, &review, is_expanded, editor_handle.clone(), cx)
                }),
                priority: 0,
            });
        }
    }
    let block_ids = editor.insert_blocks(blocks, None, cx);
    if let Some(addon) = editor.addon_mut::<ReviewAddon>() {
        addon.blocks = block_ids;
        addon.shown = threads;
    }
}

fn toggle_expanded(editor: WeakEntity<Editor>, thread_id: String, cx: &mut App) {
    editor
        .update(cx, |editor, cx| {
            if let Some(addon) = editor.addon_mut::<ReviewAddon>()
                && !addon.expanded.remove(&thread_id)
            {
                addon.expanded.insert(thread_id);
            }
            refresh_blocks(editor, true, cx);
        })
        .log_err();
}

fn wrapped_lines(text: &str) -> u32 {
    text.lines()
        .map(|line| line.chars().count().div_ceil(CARD_LINE_CHARACTERS).max(1) as u32)
        .sum::<u32>()
        .max(1)
}

/// Lines of editor text the card takes, estimated from its comments; the card scrolls when the
/// estimate falls short.
pub(crate) fn thread_height(thread: &ReviewThread, expanded: bool) -> u32 {
    if !expanded {
        return 2;
    }
    let comments = thread
        .comments
        .iter()
        .map(|comment| 1 + wrapped_lines(&comment.body))
        .sum::<u32>();
    let more = u32::from(thread.comment_count > thread.comments.len());
    // The header, the buttons and the card's padding.
    (comments + more + 3).min(30)
}

fn comment_time(created_at: &str) -> String {
    OffsetDateTime::parse(created_at, &Rfc3339)
        .map(|time| crate::git_graph::format_timestamp(time.unix_timestamp()))
        .unwrap_or_else(|_| created_at.to_string())
}

fn render_thread(
    thread: &ReviewThread,
    review: &Arc<BranchReview>,
    expanded: bool,
    editor: WeakEntity<Editor>,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors();
    let height = cx.line_height * cx.height as f32;
    let first = thread.comments.first();
    let card = v_flex()
        .id(SharedString::from(format!("review-thread-{}", thread.id)))
        .max_h(height - px(4.))
        .w(rems(CARD_WIDTH_IN_REMS))
        .max_w(cx.max_width - cx.margins.gutter.full_width() - px(16.))
        .overflow_y_scroll()
        .px_2()
        .py_1()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(colors.border_variant)
        .bg(colors.editor_subheader_background);

    let header = h_flex()
        .gap_1p5()
        .child(
            Icon::new(if thread.is_resolved {
                IconName::Check
            } else {
                IconName::Chat
            })
            .size(IconSize::XSmall)
            .color(if thread.is_resolved {
                Color::Success
            } else {
                Color::Accent
            }),
        )
        .child(
            Label::new(format!(
                "{} · #{}",
                if thread.is_resolved {
                    "Resolved"
                } else {
                    "Review comment"
                },
                review.review.number
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        );

    let card = if expanded {
        card.child(header)
            .children(thread.comments.iter().map(|comment| {
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(comment.author.clone())
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(comment_time(&comment.created_at))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(div().text_ui_sm(cx).child(comment.body.clone()))
            }))
            .when(thread.comment_count > thread.comments.len(), |card| {
                card.child(
                    Label::new(format!(
                        "{} more on GitHub",
                        thread.comment_count - thread.comments.len()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(thread_buttons(thread, review, editor))
    } else {
        let summary = first
            .map(|comment| {
                let line = comment.body.lines().next().unwrap_or_default();
                format!("{}: {line}", comment.author)
            })
            .unwrap_or_default();
        card.child(
            h_flex()
                .gap_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
                        .child(header)
                        .child(Label::new(summary).size(LabelSize::Small).truncate()),
                )
                .child(
                    Button::new(SharedString::from(format!("show-{}", thread.id)), "Show")
                        .label_size(LabelSize::Small)
                        .on_click({
                            let thread_id = thread.id.clone();
                            move |_, _, cx| toggle_expanded(editor.clone(), thread_id.clone(), cx)
                        }),
                ),
        )
    };

    div()
        .id(cx.block_id)
        .pl(cx.margins.gutter.full_width())
        .py_0p5()
        .child(card)
        .into_any_element()
}

fn thread_buttons(
    thread: &ReviewThread,
    review: &Arc<BranchReview>,
    editor: WeakEntity<Editor>,
) -> impl IntoElement {
    let id = |name: &str| SharedString::from(format!("{name}-{}", thread.id));
    h_flex()
        .gap_1()
        .when(thread.can_reply, |row| {
            row.child(
                Button::new(id("reply"), "Reply…")
                    .label_size(LabelSize::Small)
                    .on_click({
                        let editor = editor.clone();
                        let thread = thread.clone();
                        let review = review.clone();
                        move |_, window, cx| reply(&editor, &thread, &review, window, cx)
                    }),
            )
        })
        .when(
            (thread.is_resolved && thread.can_unresolve)
                || (!thread.is_resolved && thread.can_resolve),
            |row| {
                row.child(
                    Button::new(
                        id("resolve"),
                        if thread.is_resolved {
                            "Unresolve"
                        } else {
                            "Resolve"
                        },
                    )
                    .label_size(LabelSize::Small)
                    .on_click({
                        let editor = editor.clone();
                        let thread_id = thread.id.clone();
                        let resolve = !thread.is_resolved;
                        let host = review.repository.host.clone();
                        move |_, window, cx| {
                            let Some(workspace) = editor_workspace(&editor, cx) else {
                                return;
                            };
                            let client = github_client(cx);
                            let host = host.clone();
                            let thread_id = thread_id.clone();
                            run_write(
                                workspace,
                                async move {
                                    client
                                        .set_review_thread_resolved(&host, &thread_id, resolve)
                                        .await
                                },
                                window,
                                cx,
                            );
                        }
                    }),
                )
            },
        )
        .when_some(thread.url().map(ToString::to_string), |row, url| {
            row.child(
                Button::new(id("open"), "Open on GitHub")
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
        })
        .when(thread.is_resolved, |row| {
            row.child(
                Button::new(id("hide"), "Hide")
                    .label_size(LabelSize::Small)
                    .on_click({
                        let thread_id = thread.id.clone();
                        move |_, _, cx| toggle_expanded(editor.clone(), thread_id.clone(), cx)
                    }),
            )
        })
}

fn editor_workspace(editor: &WeakEntity<Editor>, cx: &App) -> Option<WeakEntity<Workspace>> {
    editor
        .upgrade()?
        .read(cx)
        .workspace()
        .map(|workspace| workspace.downgrade())
}

/// Runs a write to GitHub, then reads the pull request again so every view shows the result.
fn run_write(
    workspace: WeakEntity<Workspace>,
    write: impl Future<Output = Result<(), GitHubError>> + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| match write.await {
            Ok(()) => {
                cx.update(|window, cx| {
                    window.dispatch_action(RefreshPullRequestStatus.boxed_clone(), cx)
                })
                .log_err();
            }
            Err(error) => show_error(&workspace, error.to_string(), cx),
        })
        .detach();
}

fn show_error(workspace: &WeakEntity<Workspace>, message: String, cx: &mut AsyncWindowContext) {
    workspace
        .update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |toast, _| toast.dismiss_button(true));
            workspace.toggle_status_toast(toast, cx);
        })
        .log_err();
}

fn reply(
    editor: &WeakEntity<Editor>,
    thread: &ReviewThread,
    review: &Arc<BranchReview>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor_workspace(editor, cx) else {
        return;
    };
    let author = thread
        .comments
        .first()
        .map(|comment| comment.author.clone())
        .unwrap_or_default();
    let host = review.repository.host.clone();
    let thread_id = thread.id.clone();
    let location = format!("{}:{}", thread.path, thread.line.unwrap_or_default());
    open_text_modal(
        &workspace,
        format!("Reply to {author}"),
        location,
        vec![ModalAction::primary("Reply", {
            let workspace = workspace.clone();
            move |body, window, cx| {
                let client = github_client(cx);
                let host = host.clone();
                let thread_id = thread_id.clone();
                run_write(
                    workspace.clone(),
                    async move { client.reply_to_review_thread(&host, &thread_id, &body).await },
                    window,
                    cx,
                );
            }
        })],
        window,
        cx,
    );
}

fn add_comment(editor: WeakEntity<Editor>, row: Option<u32>, window: &mut Window, cx: &mut App) {
    let Some(editor_entity) = editor.upgrade() else {
        return;
    };
    let (path, cursor_row, workspace) = editor_entity.update(cx, |editor, cx| {
        let cursor_row = editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
            .row;
        (
            editor_path(editor, cx),
            cursor_row,
            editor.workspace().map(|workspace| workspace.downgrade()),
        )
    });
    let (Some(path), Some(workspace)) = (path, workspace) else {
        return;
    };
    let Some((_, relative, review)) = review_for_file(&path, cx) else {
        return;
    };
    let Some(commit_sha) = review.review.head_sha.clone() else {
        return;
    };
    let line = row.unwrap_or(cursor_row) + 1;
    let number = review.review.number;
    let repository = review.repository.clone();
    open_text_modal(
        &workspace,
        format!("Comment on {relative}:{line}"),
        format!(
            "Pull request #{number}. GitHub accepts comments on lines of the pull request's changes."
        ),
        vec![ModalAction::primary("Comment", {
            let workspace = workspace.clone();
            move |body, window, cx| {
                let client = github_client(cx);
                let repository = repository.clone();
                let comment = NewReviewComment {
                    commit_sha: commit_sha.clone(),
                    path: relative.clone(),
                    line,
                    body,
                };
                run_write(
                    workspace.clone(),
                    async move {
                        client
                            .add_review_comment(&repository, number, &comment)
                            .await
                    },
                    window,
                    cx,
                );
            }
        })],
        window,
        cx,
    );
}

/// The form for a review of the whole pull request.
pub(crate) fn submit_review(
    workspace: WeakEntity<Workspace>,
    work_directory: Arc<Path>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(review) = branch_review(&work_directory, cx) else {
        return;
    };
    let number = review.review.number;
    let verdict = |label: &'static str, verdict: ReviewVerdict, primary: bool| {
        let repository = review.repository.clone();
        let workspace = workspace.clone();
        let run = move |body: String, window: &mut Window, cx: &mut App| {
            let client = github_client(cx);
            let repository = repository.clone();
            run_write(
                workspace.clone(),
                async move {
                    client
                        .submit_review(&repository, number, verdict, &body)
                        .await
                },
                window,
                cx,
            );
        };
        ModalAction {
            label,
            primary,
            // A comment-only review needs words; approving or requesting changes does not.
            allow_empty: verdict != ReviewVerdict::Comment,
            run: Rc::new(run),
        }
    };
    open_text_modal(
        &workspace,
        format!("Review Pull Request #{number}"),
        review.url.clone(),
        vec![
            verdict("Comment", ReviewVerdict::Comment, false),
            verdict("Request Changes", ReviewVerdict::RequestChanges, false),
            verdict("Approve", ReviewVerdict::Approve, true),
        ],
        window,
        cx,
    );
}

/// Opens a thread's file at its line.
pub(crate) fn open_thread(
    workspace: WeakEntity<Workspace>,
    work_directory: &Path,
    thread: &ReviewThread,
    window: &mut Window,
    cx: &mut App,
) {
    let path = work_directory.join(&thread.path);
    let row = thread.line.unwrap_or(1).saturating_sub(1);
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let open = workspace.update(cx, |workspace, cx| {
        workspace.open_abs_path(path, OpenOptions::default(), window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            let item = open.await?;
            cx.update(|window, cx| {
                if let Some(editor) = item.downcast::<Editor>() {
                    editor.update(cx, |editor, cx| {
                        editor.go_to_singleton_buffer_point(Point::new(row, 0), window, cx)
                    });
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

struct ModalAction {
    label: &'static str,
    primary: bool,
    allow_empty: bool,
    run: Rc<dyn Fn(String, &mut Window, &mut App)>,
}

impl ModalAction {
    fn primary(label: &'static str, run: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        Self {
            label,
            primary: true,
            allow_empty: false,
            run: Rc::new(run),
        }
    }
}

fn open_text_modal(
    workspace: &WeakEntity<Workspace>,
    title: String,
    subtitle: String,
    actions: Vec<ModalAction>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::auto_height(4, 14, window, cx);
                editor.set_placeholder_text("Leave a comment", window, cx);
                editor
            });
            ReviewTextModal {
                title: title.into(),
                subtitle: subtitle.into(),
                editor,
                actions,
            }
        });
    });
}

pub(crate) struct ReviewTextModal {
    title: SharedString,
    subtitle: SharedString,
    editor: Entity<Editor>,
    actions: Vec<ModalAction>,
}

impl ReviewTextModal {
    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.actions.iter().position(|action| action.primary) {
            self.run(index, window, cx);
        }
    }

    fn run(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.actions.get(index) else {
            return;
        };
        let body = self.editor.read(cx).text(cx).trim().to_string();
        if body.is_empty() && !action.allow_empty {
            return;
        }
        let run = action.run.clone();
        cx.emit(DismissEvent);
        run(body, window, cx);
    }
}

impl EventEmitter<DismissEvent> for ReviewTextModal {}
impl ModalView for ReviewTextModal {}
impl Focusable for ReviewTextModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for ReviewTextModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.editor.focus_handle(cx);
        v_flex()
            .key_context("ReviewTextModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(40.))
            .child(
                v_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Icon::new(IconName::Chat).size(IconSize::XSmall))
                            .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall)),
                    )
                    .child(
                        Label::new(self.subtitle.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
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
                    .child(Button::new("cancel-review-text", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)),
                    ))
                    .children(self.actions.iter().enumerate().map(|(index, action)| {
                        Button::new(("review-text-action", index), action.label)
                            .when(action.primary, |button| {
                                button.style(ButtonStyle::Filled).key_binding(
                                    KeyBinding::for_action_in(&Confirm, &focus_handle, cx),
                                )
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run(index, window, cx)
                            }))
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        commit_operations::tests::{create_repository, init_test, open, run_git, wait_for},
        pull_requests::{
            PullRequestIndicator, set_github_client,
            tests::{FixedToken, json_response},
        },
    };
    use evb_github::{GitHubClient, ReviewComment};
    use futures::AsyncReadExt as _;
    use git::GitHostingProviderRegistry;
    use gpui::TestAppContext;
    use http_client::FakeHttpClient;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use util::test::TempTree;

    /// A pull request with one review thread on line 2 of `e.txt`: the thread shows under the
    /// line, a gutter comment goes to the head commit, and a reply goes to the thread.
    #[gpui::test]
    async fn test_review_threads_in_the_editor(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        create_repository(root);
        run_git(
            root,
            &["remote", "add", "origin", "https://github.com/evb0110/evb-ide.git"],
        );
        std::fs::write(root.join("e.txt"), "one\ntwo\nthree\n").expect("writing a fixture file");
        run_git(root, &["add", "e.txt"]);
        run_git(root, &["commit", "-q", "-m", "Add E"]);
        run_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run_git(root, &["config", "branch.main.remote", "origin"]);
        run_git(root, &["config", "branch.main.merge", "refs/heads/main"]);

        let writes = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let http = FakeHttpClient::create({
            let writes = writes.clone();
            move |mut request| {
                let writes = writes.clone();
                async move {
                    let uri = request.uri().to_string();
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    let body: Value = serde_json::from_str(&body)?;
                    let query = body["query"].as_str().unwrap_or_default().to_string();
                    let data = if query.contains("BranchPullRequests") {
                        json!({ "repository": {
                            "defaultBranchRef": { "name": "main" },
                            "pullRequests": { "nodes": [{
                                "number": 12, "title": "E", "url": "https://github.com/evb0110/evb-ide/pull/12",
                                "state": "OPEN", "isDraft": false, "baseRefName": "main", "headRefOid": "head",
                                "updatedAt": "2026-10-31T10:00:00Z", "isCrossRepository": false,
                                "headRepositoryOwner": { "login": "evb0110" }
                            }] }
                        } })
                    } else if query.contains("PullRequestStatus") {
                        json!({ "repository": { "pullRequest": {
                            "mergeable": "MERGEABLE", "reviewDecision": null, "commits": { "nodes": [] }
                        } } })
                    } else if query.contains("ReviewThreads") {
                        json!({ "repository": { "pullRequest": {
                            "id": "PR_12", "headRefOid": "head",
                            "reviewThreads": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [{
                                    "id": "T1", "isResolved": false, "isOutdated": false,
                                    "path": "e.txt", "line": 2,
                                    "viewerCanReply": true, "viewerCanResolve": true, "viewerCanUnresolve": false,
                                    "comments": { "totalCount": 1, "nodes": [{
                                        "id": "C1", "author": { "login": "reviewer" }, "body": "Why two?",
                                        "createdAt": "2026-10-31T10:00:00Z",
                                        "url": "https://github.com/evb0110/evb-ide/pull/12#discussion_r1"
                                    }] }
                                }]
                            }
                        } } })
                    } else {
                        writes.lock().expect("recording a write").push((uri.clone(), body.clone()));
                        json!({})
                    };
                    if uri.ends_with("/comments") {
                        writes.lock().expect("recording a write").push((uri, body));
                        return Ok(json_response(201, json!({ "id": 1 })));
                    }
                    Ok(json_response(200, json!({ "data": data })))
                }
            }
        });
        cx.update(|cx| {
            GitHostingProviderRegistry::default_global(cx).register_hosting_provider(Arc::new(
                git_hosting_providers::Github::public_instance(),
            ));
            set_github_client(
                GitHubClient::new(http, Arc::new(FixedToken), cx.background_executor().clone()),
                cx,
            );
        });

        let (mut cx, _repository, workspace) = open(root, cx).await;
        let workspace = workspace.upgrade().expect("the workspace is open");
        let _indicator = cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                cx.new(|cx| PullRequestIndicator::new(workspace, cx))
            })
        });
        let open_file = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_abs_path(root.join("e.txt"), OpenOptions::default(), window, cx)
            })
        });
        let item = open_file.await.expect("opening e.txt");
        let editor = item.downcast::<Editor>().expect("an editor");
        // `git_ui::init` attaches this to every new editor.
        editor.update(&mut cx, |editor, cx| register_editor(editor, cx));
        let shown = wait_for(&mut cx, |cx| {
            let addon = editor.read(cx).addon::<ReviewAddon>()?;
            (addon.blocks.len() == 1).then(|| addon.shown.clone())
        });
        assert_eq!(shown[0].id, "T1");
        assert_eq!(shown[0].line, Some(2));

        cx.update(|window, cx| {
            let focus_handle = editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
            focus_handle.dispatch_action(
                &git::AddPullRequestComment { row: Some(0) },
                window,
                cx,
            );
        });
        let modal = wait_for(&mut cx, |cx| workspace.read(cx).active_modal::<ReviewTextModal>(cx));
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| editor.set_text("Nit", window, cx));
            modal.confirm(&Confirm, window, cx);
        });
        let comment = wait_for(&mut cx, |_| {
            writes.lock().expect("reading writes").iter().find(|(uri, _)| uri.ends_with("/comments")).cloned()
        });
        assert_eq!(
            comment,
            (
                "https://api.github.com/repos/evb0110/evb-ide/pulls/12/comments".to_string(),
                json!({ "body": "Nit", "commit_id": "head", "path": "e.txt", "line": 1, "side": "RIGHT" })
            )
        );

        let review = cx.update(|_, cx| branch_review(root, cx)).expect("a review");
        cx.update(|window, cx| reply(&editor.downgrade(), &shown[0], &review, window, cx));
        let modal = wait_for(&mut cx, |cx| workspace.read(cx).active_modal::<ReviewTextModal>(cx));
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| editor.set_text("Because three", window, cx));
            modal.confirm(&Confirm, window, cx);
        });
        let reply = wait_for(&mut cx, |_| {
            writes
                .lock()
                .expect("reading writes")
                .iter()
                .find(|(_, body)| body["query"].as_str().is_some_and(|query| query.contains("ReplyToReviewThread")))
                .cloned()
        });
        assert_eq!(reply.1["variables"], json!({ "threadId": "T1", "body": "Because three" }));
    }

    fn thread(bodies: &[&str], count: usize) -> ReviewThread {
        ReviewThread {
            id: "T".into(),
            path: "src/main.rs".into(),
            line: Some(3),
            is_resolved: false,
            is_outdated: false,
            can_reply: true,
            can_resolve: true,
            can_unresolve: false,
            comments: bodies
                .iter()
                .map(|body| ReviewComment {
                    id: String::new(),
                    author: "evb".into(),
                    body: body.to_string(),
                    created_at: String::new(),
                    url: String::new(),
                })
                .collect(),
            comment_count: count,
        }
    }

    #[test]
    fn thread_heights_follow_their_comments() {
        assert_eq!(thread_height(&thread(&["Short"], 1), false), 2);
        assert_eq!(thread_height(&thread(&["Short"], 1), true), 5);
        let long = "x".repeat(CARD_LINE_CHARACTERS * 2 + 1);
        assert_eq!(thread_height(&thread(&["One\nTwo", &long], 5), true), 3 + 3 + 4 + 1);
        let many = vec!["line"; 40];
        assert_eq!(thread_height(&thread(&many, 40), true), 30);
    }
}

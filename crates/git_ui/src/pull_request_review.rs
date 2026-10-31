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
    pub work_directory: Arc<Path>,
    /// The local branch the review was read for.
    pub branch: String,
    pub repository: RepositoryLocator,
    pub url: String,
    pub review: PullRequestReview,
}

/// Whether `review` is for the pull request of the branch checked out now. A newer read of the
/// same pull request still counts; another branch or pull request does not, and a write must not
/// reach a pull request the user no longer looks at.
fn is_current(review: &BranchReview, cx: &App) -> bool {
    branch_review(&review.work_directory, cx).is_some_and(|current| {
        current.branch == review.branch
            && current.repository == review.repository
            && current.review.number == review.review.number
            && current.review.pull_request_id == review.review.pull_request_id
    })
}

fn stale_review_error(review: &BranchReview) -> GitHubError {
    GitHubError::Response {
        host: review.repository.host.clone(),
        status: 0,
        messages: vec![
            "The pull request view is out of date; wait for it to refresh, then try again.".into(),
        ],
    }
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

/// The review covering a file, and the file's path in the repository. A file inside a nested
/// repository (a submodule) belongs to the innermost one.
fn review_for_file(path: &Path, cx: &App) -> Option<(Arc<Path>, String, Arc<BranchReview>)> {
    let cache = cx.try_global::<ReviewCache>()?;
    let (work_directory, review) = cache
        .0
        .iter()
        .filter(|(work_directory, _)| path.starts_with(work_directory))
        .max_by_key(|(work_directory, _)| work_directory.components().count())?;
    let relative = path
        .strip_prefix(work_directory)
        .ok()?
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some((work_directory.clone(), relative, review.clone()))
}

/// Lines that are the same in the pull request's head commit and in the buffer, so threads and
/// new comments land on the code they are about even when the buffer has local changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineMap {
    /// Runs of equal lines: (first head line, first buffer line, length), zero-based.
    runs: Vec<(u32, u32, u32)>,
}

impl LineMap {
    pub(crate) fn new(head: &str, buffer: &str) -> Self {
        let head_lines = head.lines().collect::<Vec<_>>();
        let buffer_lines = buffer.lines().collect::<Vec<_>>();
        let diff = similar::TextDiff::configure().diff_slices(&head_lines, &buffer_lines);
        let runs = diff
            .ops()
            .iter()
            .filter_map(|operation| match *operation {
                similar::DiffOp::Equal {
                    old_index,
                    new_index,
                    len,
                } => Some((old_index as u32, new_index as u32, len as u32)),
                _ => None,
            })
            .collect();
        Self { runs }
    }

    pub(crate) fn head_to_buffer(&self, head_line: u32) -> Option<u32> {
        self.runs.iter().find_map(|&(head, buffer, len)| {
            (head..head + len)
                .contains(&head_line)
                .then(|| buffer + head_line - head)
        })
    }

    pub(crate) fn buffer_to_head(&self, buffer_line: u32) -> Option<u32> {
        self.runs.iter().find_map(|&(head, buffer, len)| {
            (buffer..buffer + len)
                .contains(&buffer_line)
                .then(|| head + buffer_line - buffer)
        })
    }
}

/// The file as the pull request's head commit has it; `text` is `None` when the commit is not
/// in the local repository.
#[derive(Clone, Debug)]
struct HeadText {
    sha: String,
    path: String,
    text: Option<Arc<str>>,
    read_at: std::time::Instant,
}

/// A failed read is tried again after this, once something may have fetched the commit.
const HEAD_TEXT_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
/// Edits settle this long before threads are mapped onto the buffer again.
const REMAP_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

fn editor_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Some(file.as_local()?.abs_path(cx))
}

struct ReviewAddon {
    blocks: Vec<CustomBlockId>,
    /// What the blocks show, so unrelated cache updates leave them alone.
    shown: Vec<(ReviewThread, u32)>,
    head_text: Option<HeadText>,
    head_text_task: Option<gpui::Task<()>>,
    remap_task: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
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
    let mut subscriptions = vec![cx.subscribe(
        &editor.buffer().clone(),
        |editor, _, event: &multi_buffer::Event, cx| match event {
            // Threads follow their lines as the buffer changes.
            multi_buffer::Event::Edited { .. } | multi_buffer::Event::Reloaded => {
                schedule_remap(editor, cx)
            }
            multi_buffer::Event::FileHandleChanged => refresh_blocks(editor, false, cx),
            _ => {}
        },
    )];
    if let Some(project) = editor.project() {
        // A fetch can bring the head commit whose file could not be read.
        let git_store = project.read(cx).git_store().clone();
        subscriptions.push(cx.subscribe(
            &git_store,
            |editor, _, event: &project::git_store::GitStoreEvent, cx| {
                if matches!(event, project::git_store::GitStoreEvent::RepositoryUpdated(..))
                    && editor.addon::<ReviewAddon>().is_some_and(|addon| {
                        addon
                            .head_text
                            .as_ref()
                            .is_some_and(|head| head.text.is_none())
                    })
                {
                    refresh_blocks(editor, false, cx);
                }
            },
        ));
    }
    editor.register_addon(ReviewAddon {
        blocks: Vec::new(),
        shown: Vec::new(),
        head_text: None,
        head_text_task: None,
        remap_task: None,
        _subscriptions: subscriptions,
        expanded: HashSet::default(),
        comment_action: None,
        _cache_observer: observer,
    });
    refresh_blocks(editor, false, cx);
}

/// Loads the file at the pull request's head commit once per commit and path.
fn ensure_head_text(
    editor: &mut Editor,
    work_directory: &Path,
    relative: &str,
    sha: &str,
    cx: &mut Context<Editor>,
) -> Option<Arc<str>> {
    let addon = editor.addon::<ReviewAddon>()?;
    if let Some(head) = &addon.head_text
        && head.sha == sha
        && head.path == relative
        && (head.text.is_some() || head.read_at.elapsed() < HEAD_TEXT_RETRY)
    {
        return head.text.clone();
    }
    if addon.head_text_task.is_some() {
        return None;
    }
    let repository = editor
        .project()?
        .read(cx)
        .git_store()
        .read(cx)
        .repositories()
        .values()
        .find(|repository| repository.read(cx).work_directory_abs_path.as_ref() == work_directory)?
        .clone();
    let show = repository.update(cx, |repository, _| {
        repository.read_git_command(vec!["show".into(), format!("{sha}:{relative}")])
    });
    let (sha, relative) = (sha.to_string(), relative.to_string());
    let task = cx.spawn(async move |editor, cx| {
        let text = match show.await {
            Ok(Ok(text)) => Some(Arc::<str>::from(text)),
            _ => None,
        };
        editor
            .update(cx, |editor, cx| {
                if let Some(addon) = editor.addon_mut::<ReviewAddon>() {
                    addon.head_text = Some(HeadText {
                        sha,
                        path: relative,
                        text,
                        read_at: std::time::Instant::now(),
                    });
                    addon.head_text_task = None;
                }
                refresh_blocks(editor, true, cx);
            })
            .log_err();
    });
    if let Some(addon) = editor.addon_mut::<ReviewAddon>() {
        addon.head_text_task = Some(task);
    }
    None
}

fn buffer_text(editor: &Editor, cx: &App) -> Option<String> {
    Some(editor.buffer().read(cx).as_singleton()?.read(cx).text())
}

fn refresh_blocks(editor: &mut Editor, force: bool, cx: &mut Context<Editor>) {
    let found = editor_path(editor, cx).and_then(|path| review_for_file(&path, cx));
    // Threads on deleted lines count the base commit's lines, and outdated ones have none; both
    // stay in the status bar list, which opens them on GitHub.
    let mut threads: Vec<(ReviewThread, u32)> = Vec::new();
    if let Some((work_directory, relative, review)) = &found
        && let Some(sha) = review.review.head_sha.clone()
        && let Some(head) = ensure_head_text(editor, work_directory, relative, &sha, cx)
        && let Some(text) = buffer_text(editor, cx)
    {
        let map = LineMap::new(&head, &text);
        threads = review
            .review
            .threads
            .iter()
            .filter(|thread| &thread.path == relative && thread.on_head)
            .filter_map(|thread| {
                let row = map.head_to_buffer(thread.line?.checked_sub(1)?)?;
                Some((thread.clone(), row))
            })
            .collect();
    }

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
        for (thread, row) in &threads {
            let row = (*row).min(max_row);
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

fn schedule_remap(editor: &mut Editor, cx: &mut Context<Editor>) {
    if editor
        .addon::<ReviewAddon>()
        .is_none_or(|addon| addon.head_text.is_none())
    {
        return;
    }
    let task = cx.spawn(async move |editor, cx| {
        cx.background_executor().timer(REMAP_DELAY).await;
        editor
            .update(cx, |editor, cx| refresh_blocks(editor, false, cx))
            .log_err();
    });
    if let Some(addon) = editor.addon_mut::<ReviewAddon>() {
        addon.remap_task = Some(task);
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
                        let review = review.clone();
                        move |_, window, cx| {
                            let Some(workspace) = editor_workspace(&editor, cx) else {
                                return;
                            };
                            let client = github_client(cx);
                            let host = review.repository.host.clone();
                            let thread_id = thread_id.clone();
                            run_write(
                                workspace,
                                &review,
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

/// Runs a write to GitHub for `review`, then reads the pull request again so every view shows
/// the result. Returns `false`, writing nothing, when the review is no longer the current one.
fn run_write(
    workspace: WeakEntity<Workspace>,
    review: &BranchReview,
    write: impl Future<Output = Result<(), GitHubError>> + 'static,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if !is_current(review, cx) {
        let error = stale_review_error(review);
        window
            .spawn(cx, async move |cx| show_error(&workspace, error.to_string(), cx))
            .detach();
        return false;
    }
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
    true
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
    let review = review.clone();
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
                    &review,
                    async move { client.reply_to_review_thread(&host, &thread_id, &body).await },
                    window,
                    cx,
                )
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
    let (path, cursor_row, workspace, head_text, text) = editor_entity.update(cx, |editor, cx| {
        let cursor_row = editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
            .row;
        (
            editor_path(editor, cx),
            cursor_row,
            editor.workspace().map(|workspace| workspace.downgrade()),
            editor
                .addon::<ReviewAddon>()
                .and_then(|addon| addon.head_text.clone()),
            buffer_text(editor, cx),
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
    // GitHub counts lines of the head commit's file; the buffer may differ from it.
    let buffer_row = row.unwrap_or(cursor_row);
    let head = head_text
        .filter(|head| head.sha == commit_sha && head.path == relative)
        .and_then(|head| head.text);
    let head_row = head
        .zip(text)
        .and_then(|(head, text)| LineMap::new(&head, &text).buffer_to_head(buffer_row));
    let Some(head_row) = head_row else {
        let message = format!(
            "Line {} is not in pull request #{} as pushed: it changed locally, or the pull request's commit is not fetched. Comment on an unchanged line, or push first.",
            buffer_row + 1,
            review.review.number
        );
        window
            .spawn(cx, async move |cx| show_error(&workspace, message, cx))
            .detach();
        return;
    };
    let line = head_row + 1;
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
                // A newer push moves the head; the line was mapped against the old one.
                let head_now = branch_review(&review.work_directory, cx)
                    .and_then(|current| current.review.head_sha.clone());
                if head_now.as_deref() != Some(commit_sha.as_str()) {
                    let error = stale_review_error(&review);
                    let workspace = workspace.clone();
                    window
                        .spawn(cx, async move |cx| show_error(&workspace, error.to_string(), cx))
                        .detach();
                    return false;
                }
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
                    &review,
                    async move {
                        client
                            .add_review_comment(&repository, number, &comment)
                            .await
                    },
                    window,
                    cx,
                )
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
        let review = review.clone();
        let run = move |body: String, window: &mut Window, cx: &mut App| -> bool {
            let client = github_client(cx);
            let repository = repository.clone();
            run_write(
                workspace.clone(),
                &review,
                async move {
                    client
                        .submit_review(&repository, number, verdict, &body)
                        .await
                },
                window,
                cx,
            )
        };
        ModalAction {
            label,
            primary,
            // GitHub requires words for a comment or a request for changes.
            allow_empty: verdict == ReviewVerdict::Approve,
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
    let thread_id = thread.id.clone();
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let open = workspace.update(cx, |workspace, cx| {
        workspace.open_abs_path(path, OpenOptions::default(), window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            let item = open.await?;
            let Some(editor) = item.downcast::<Editor>() else {
                return anyhow::Ok(());
            };
            // A file opened just now reads its pull request version first; until then its
            // threads have no place in the buffer.
            for _ in 0..30 {
                let mapped = editor.read_with(cx, |editor, _| {
                    editor.addon::<ReviewAddon>().is_none_or(|addon| {
                        addon.head_text_task.is_none() && addon.head_text.is_some()
                    })
                });
                if mapped {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
            cx.update(|window, cx| {
                {
                    editor.update(cx, |editor, cx| {
                        // The editor knows where the line is now if the buffer has changed.
                        let row = editor
                            .addon::<ReviewAddon>()
                            .and_then(|addon| {
                                addon
                                    .shown
                                    .iter()
                                    .find(|(shown, _)| shown.id == thread_id)
                                    .map(|(_, row)| *row)
                            })
                            .unwrap_or(row);
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
    /// Returns whether it sent the text; the form stays open, keeping it, when not.
    run: Rc<dyn Fn(String, &mut Window, &mut App) -> bool>,
}

impl ModalAction {
    fn primary(
        label: &'static str,
        run: impl Fn(String, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
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
        if run(body, window, cx) {
            cx.emit(DismissEvent);
        }
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

        let head = run_git(root, &["rev-parse", "HEAD"]);

        let writes = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let http = FakeHttpClient::create({
            let writes = writes.clone();
            let head = head.clone();
            move |mut request| {
                let writes = writes.clone();
                let head = head.clone();
                async move {
                    let uri = request.uri().to_string();
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    let body: Value = if body.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str(&body)?
                    };
                    if uri.ends_with("/repos/evb0110/evb-ide") {
                        return Ok(json_response(200, json!({ "default_branch": "main" })));
                    }
                    if uri.contains("/pulls?") {
                        return Ok(json_response(
                            200,
                            json!([{
                                "number": 12, "title": "E", "html_url": "https://github.com/evb0110/evb-ide/pull/12",
                                "state": "open", "draft": false, "merged_at": null,
                                "base": { "ref": "main" },
                                "head": { "sha": head, "repo": { "full_name": "evb0110/evb-ide" } }
                            }]),
                        ));
                    }
                    if uri.ends_with("/comments") {
                        writes.lock().expect("recording a write").push((uri, body));
                        return Ok(json_response(201, json!({ "id": 1 })));
                    }
                    let query = body["query"].as_str().unwrap_or_default().to_string();
                    let comment = |id: &str, body: &str| json!({
                        "id": id, "author": { "login": "reviewer" }, "body": body,
                        "createdAt": "2026-10-31T10:00:00Z",
                        "url": format!("https://github.com/evb0110/evb-ide/pull/12#discussion_{id}")
                    });
                    let data = if query.contains("PullRequestStatus") {
                        json!({ "repository": { "pullRequest": {
                            "mergeable": "MERGEABLE", "reviewDecision": null, "commits": { "nodes": [] }
                        } } })
                    } else if query.contains("ReviewThreads") {
                        json!({ "repository": { "pullRequest": {
                            "id": "PR_12", "headRefOid": head,
                            "reviewThreads": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [
                                    {
                                        "id": "T1", "isResolved": false, "isOutdated": false,
                                        "path": "e.txt", "line": 2, "diffSide": "RIGHT",
                                        "viewerCanReply": true, "viewerCanResolve": true, "viewerCanUnresolve": false,
                                        "comments": { "totalCount": 1, "nodes": [comment("C1", "Why two?")] }
                                    },
                                    {
                                        // On a deleted line: base line numbers, not this file's.
                                        "id": "T2", "isResolved": false, "isOutdated": false,
                                        "path": "e.txt", "line": 3, "diffSide": "LEFT",
                                        "viewerCanReply": true, "viewerCanResolve": true, "viewerCanUnresolve": false,
                                        "comments": { "totalCount": 1, "nodes": [comment("C2", "Why remove it?")] }
                                    }
                                ]
                            }
                        } } })
                    } else if query.contains("ReplyToReviewThread") {
                        writes.lock().expect("recording a write").push((uri, body));
                        json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "C3" } } })
                    } else {
                        panic!("unexpected request {query}")
                    };
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
        assert_eq!(
            shown.iter().map(|(thread, row)| (thread.id.as_str(), *row)).collect::<Vec<_>>(),
            vec![("T1", 1)]
        );

        // A line added locally above the thread: the thread follows its line, and comments count
        // the pushed file's lines.
        editor.update_in(&mut cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([Point::new(0, 0)..Point::new(0, 0)])
            });
            editor.insert("zero\n", window, cx);
        });
        // The edit remaps the threads once it settles.
        cx.executor().advance_clock(REMAP_DELAY * 2);
        cx.run_until_parked();
        let shown_rows = cx.update(|_, cx| {
            editor
                .read(cx)
                .addon::<ReviewAddon>()
                .map(|addon| addon.shown.iter().map(|(_, row)| *row).collect::<Vec<_>>())
        });
        assert_eq!(shown_rows, Some(vec![2]));

        let comment_on = |row: u32, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let focus_handle = editor.focus_handle(cx);
                window.focus(&focus_handle, cx);
                focus_handle.dispatch_action(
                    &git::AddPullRequestComment { row: Some(row) },
                    window,
                    cx,
                );
            });
        };
        comment_on(0, &mut cx);
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| workspace.read(cx).active_modal::<ReviewTextModal>(cx)).is_none(),
            "the local line is not in the pull request"
        );

        comment_on(2, &mut cx);
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
                json!({ "body": "Nit", "commit_id": head, "path": "e.txt", "line": 2, "side": "RIGHT" })
            )
        );

        let review = cx.update(|_, cx| branch_review(root, cx)).expect("a review");
        cx.update(|window, cx| reply(&editor.downgrade(), &shown[0].0, &review, window, cx));
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
            on_head: true,
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

    #[gpui::test]
    fn writes_need_the_same_branch_and_pull_request(cx: &mut TestAppContext) {
        let review = BranchReview {
            work_directory: Arc::from(Path::new("/repository")),
            branch: "feature".into(),
            repository: RepositoryLocator {
                host: "github.com".into(),
                owner: "o".into(),
                name: "r".into(),
            },
            url: "https://github.com/o/r/pull/1".into(),
            review: PullRequestReview {
                pull_request_id: "PR_1".into(),
                number: 1,
                head_sha: Some("a".into()),
                threads: Vec::new(),
            },
        };
        cx.update(|cx| {
            set_branch_review(review.work_directory.clone(), Some(review.clone()), cx);
            assert!(is_current(&review, cx));
            // A newer read of the same pull request (new commits, new threads) keeps it current.
            let mut newer = review.clone();
            newer.review.head_sha = Some("b".into());
            newer.review.threads.push(thread(&["New"], 1));
            set_branch_review(review.work_directory.clone(), Some(newer), cx);
            assert!(is_current(&review, cx));
            // Another branch does not.
            let mut other = review.clone();
            other.branch = "other".into();
            set_branch_review(review.work_directory.clone(), Some(other), cx);
            assert!(!is_current(&review, cx));
            set_branch_review(review.work_directory.clone(), None, cx);
            assert!(!is_current(&review, cx));
        });
    }

    #[test]
    fn line_map_follows_local_changes() {
        let map = LineMap::new("a\nb\nc\nd\n", "new\na\nb\nchanged\nd");
        assert_eq!(
            (0..4).map(|line| map.head_to_buffer(line)).collect::<Vec<_>>(),
            vec![Some(1), Some(2), None, Some(4)]
        );
        assert_eq!(
            (0..5).map(|line| map.buffer_to_head(line)).collect::<Vec<_>>(),
            vec![None, Some(0), Some(1), None, Some(3)]
        );
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

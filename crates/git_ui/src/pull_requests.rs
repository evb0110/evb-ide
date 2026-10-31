use crate::{
    commit_operations::args,
    pull_request_review::{BranchReview, branch_review, set_branch_review},
};
use editor::Editor;
use evb_github::{
    BranchHead, BranchPullRequests, CheckStatus, ChecksSummary, GitHubClient, GitHubError,
    Mergeable, NewPullRequest, PullRequest, PullRequestState, RepositoryLocator, ReviewDecision,
    SystemCredentials,
};
use git::{GitHostingProviderRegistry, parse_git_remote_url, repository::UpstreamTracking};
use gpui::{
    Action as _, Anchor, App, AsyncWindowContext, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Global, SharedString, Subscription, Task, WeakEntity, Window, actions,
};
use menu::{Cancel, Confirm};
use notifications::status_toast::StatusToast;
use project::git_store::{GitStore, GitStoreEvent, Repository, RepositoryId};
use settings::{Settings as _, SettingsStore};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};
use ui::{
    ButtonLike, ContextMenu, KeyBinding, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ModalView, StatusBarSettings, StatusItemView, Workspace, item::ItemHandle,
};

actions!(
    git,
    [
        /// Asks GitHub again for the pull request of the current branch.
        RefreshPullRequestStatus,
    ]
);

/// While checks run, their results are what the indicator is watched for.
const PENDING_CHECKS_INTERVAL: Duration = Duration::from_secs(30);
const OPEN_PULL_REQUEST_INTERVAL: Duration = Duration::from_secs(2 * 60);
const IDLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// No credential is unlikely to change soon, and `gh` should not run every few seconds.
const MISSING_CREDENTIAL_INTERVAL: Duration = Duration::from_secs(10 * 60);
const FAILURE_BASE_DELAY: Duration = Duration::from_secs(30);
const FAILURE_MAX_DELAY: Duration = Duration::from_secs(15 * 60);
const MINIMUM_RATE_LIMIT_DELAY: Duration = Duration::from_secs(60);
const MENU_CHECK_LIMIT: usize = 12;
const MENU_THREAD_LIMIT: usize = 15;

struct GitHubClientHandle(Arc<GitHubClient>);

impl Global for GitHubClientHandle {}

pub fn set_github_client(client: Arc<GitHubClient>, cx: &mut App) {
    cx.set_global(GitHubClientHandle(client));
}

pub(crate) fn github_client(cx: &mut App) -> Arc<GitHubClient> {
    if let Some(handle) = cx.try_global::<GitHubClientHandle>() {
        return handle.0.clone();
    }
    let executor = cx.background_executor().clone();
    let client = GitHubClient::new(
        cx.http_client(),
        Arc::new(SystemCredentials::new(executor.clone())),
        executor,
    );
    set_github_client(client.clone(), cx);
    client
}

/// What is known about each GitHub branch, for actions outside the indicator.
#[derive(Default)]
struct PullRequestCache(collections::HashMap<(Arc<Path>, String), BranchPullRequests>);

impl Global for PullRequestCache {}

pub(crate) struct KnownBranch {
    pub open_pull_request: Option<u64>,
    pub default_branch: Option<String>,
}

/// The last pull request status read for the repository's current branch, if it is on GitHub.
pub(crate) fn known_branch(repository: &Repository, cx: &App) -> Option<KnownBranch> {
    let branch = repository.branch.as_ref()?.name().to_string();
    let cache = cx.try_global::<PullRequestCache>()?;
    let known = cache
        .0
        .get(&(repository.work_directory_abs_path.clone(), branch))?;
    Some(KnownBranch {
        open_pull_request: known
            .pull_request
            .as_ref()
            .filter(|pull_request| pull_request.state.is_open())
            .map(|pull_request| pull_request.number),
        default_branch: known.default_branch.clone(),
    })
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &RefreshPullRequestStatus, _, cx| {
        if let Some(indicator) = workspace
            .status_bar()
            .read(cx)
            .item_of_type::<PullRequestIndicator>()
        {
            indicator.update(cx, |indicator, cx| indicator.refresh(cx));
        }
    });
}

/// The branch's GitHub repositories, chosen the way `gh` chooses them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedBranch {
    pub head: BranchHead,
    pub local_branch: String,
    /// The remote of the target repository, when one points at it.
    pub target_remote: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RemoteConfig {
    name: String,
    url: Option<String>,
    /// `gh repo set-default`: `base` on the remote of the chosen repository, or the chosen
    /// repository's `owner/name` when no remote points at it.
    gh_resolved: Option<String>,
}

fn parse_remote_config(output: &str) -> Vec<RemoteConfig> {
    let mut remotes: Vec<RemoteConfig> = Vec::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let Some(key) = key.strip_prefix("remote.") else {
            continue;
        };
        let (name, field) = match key.rsplit_once('.') {
            Some((name, field @ ("url" | "gh-resolved"))) => (name, field),
            _ => continue,
        };
        let index = match remotes.iter().position(|remote| remote.name == name) {
            Some(index) => index,
            None => {
                remotes.push(RemoteConfig {
                    name: name.to_string(),
                    ..RemoteConfig::default()
                });
                remotes.len() - 1
            }
        };
        let remote = &mut remotes[index];
        match field {
            "url" if remote.url.is_none() => remote.url = Some(value.trim().to_string()),
            "gh-resolved" => remote.gh_resolved = Some(value.trim().to_string()),
            _ => {}
        }
    }
    remotes
}

fn locate(registry: &Arc<GitHostingProviderRegistry>, url: &str) -> Option<RepositoryLocator> {
    let (provider, parsed) = parse_git_remote_url(registry.clone(), url)?;
    if !provider.is_github() {
        return None;
    }
    Some(RepositoryLocator {
        host: provider.base_url().host_str()?.to_string(),
        owner: parsed.owner.to_string(),
        name: parsed.repo.to_string(),
    })
}

/// The repository pull requests target and the remote pointing at it. `gh repo set-default`
/// decides when set; otherwise gh prefers `upstream`, `github`, then `origin`.
fn target_repository(
    remotes: &[RemoteConfig],
    registry: &Arc<GitHostingProviderRegistry>,
    head_remote: Option<&str>,
) -> Option<(RepositoryLocator, Option<String>)> {
    for remote in remotes {
        let (Some(resolved), Some(url)) = (remote.gh_resolved.as_deref(), remote.url.as_deref())
        else {
            continue;
        };
        let located = locate(registry, url)?;
        if resolved == "base" {
            return Some((located, Some(remote.name.clone())));
        }
        // `owner/name`, or `host/owner/name` for another host.
        let parts = resolved.split('/').collect::<Vec<_>>();
        let (host, owner, name) = match parts.as_slice() {
            [owner, name] => (located.host, *owner, *name),
            [host, owner, name] => (host.to_string(), *owner, *name),
            _ => continue,
        };
        let target = RepositoryLocator {
            host,
            owner: owner.to_string(),
            name: name.to_string(),
        };
        let pointing = remotes
            .iter()
            .find(|remote| {
                remote
                    .url
                    .as_deref()
                    .and_then(|url| locate(registry, url))
                    .is_some_and(|located| located == target)
            })
            .map(|remote| remote.name.clone());
        return Some((target, pointing));
    }
    let remote = ["upstream", "github", "origin"]
        .iter()
        .find_map(|name| remotes.iter().find(|remote| remote.name == *name))
        .or_else(|| head_remote.and_then(|name| remotes.iter().find(|remote| remote.name == name)))
        .or_else(|| remotes.first())?;
    Some((
        locate(registry, remote.url.as_deref()?)?,
        Some(remote.name.clone()),
    ))
}

/// Where the branch is pushed, by the git panel's rule: to the branch's push remote
/// (`pushRemote`, `remote.pushDefault`, the upstream's remote, as `git for-each-ref` reports it),
/// under the upstream branch's name, or the local name without an upstream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PushTarget {
    pub remote: Option<String>,
    pub branch: Option<String>,
}

fn parse_push_remote(output: &str) -> Option<String> {
    let remote = output.lines().next().unwrap_or_default().trim();
    (!remote.is_empty()).then(|| remote.to_string())
}

/// A branch a push just sent, where the push sent it.
pub(crate) struct PushedBranch {
    pub repository: Entity<Repository>,
    pub branch: String,
    pub remote: String,
    pub remote_branch: String,
}

fn resolve_from_remotes(
    remotes: &[RemoteConfig],
    registry: &Arc<GitHostingProviderRegistry>,
    local_branch: String,
    push: &PushTarget,
) -> Option<ResolvedBranch> {
    let (target, target_remote) = target_repository(remotes, registry, push.remote.as_deref())?;
    // A branch without a push destination yet will go to origin, else to the target.
    let head = match push
        .remote
        .as_deref()
        .or_else(|| remotes.iter().any(|remote| remote.name == "origin").then_some("origin"))
        .and_then(|name| remotes.iter().find(|remote| remote.name == name))
        .and_then(|remote| remote.url.as_deref())
    {
        Some(url) => locate(registry, url)?,
        None => target.clone(),
    };
    Some(ResolvedBranch {
        head: BranchHead {
            target,
            head,
            branch: push.branch.clone().unwrap_or_else(|| local_branch.clone()),
        },
        local_branch,
        target_remote,
    })
}

/// Resolves to `None` when the repository has no branch checked out or no GitHub remote.
/// `pushed` is where a push just went, which then needs no working out.
pub(crate) fn resolve_branch(
    repository: &Entity<Repository>,
    pushed: Option<PushTarget>,
    cx: &mut App,
) -> Task<Option<ResolvedBranch>> {
    let (branch, origin_url, upstream_url) = {
        let repository = repository.read(cx);
        (
            repository.branch.clone(),
            repository.remote_origin_url.clone(),
            repository.remote_upstream_url.clone(),
        )
    };
    let Some(branch) = branch else {
        return Task::ready(None);
    };
    let local_branch = branch.name().to_string();
    let upstream = branch
        .upstream
        .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)));
    let upstream_remote = upstream
        .as_ref()
        .and_then(|upstream| upstream.remote_name())
        .map(ToString::to_string);
    let remote_branch = upstream
        .as_ref()
        .and_then(|upstream| upstream.branch_name())
        .map(ToString::to_string)
        .unwrap_or_else(|| local_branch.clone());
    let (config, push) = repository.update(cx, |repository, _| {
        (
            repository.read_git_command(args([
                "config",
                "--get-regexp",
                r"^remote\..*\.(url|gh-resolved)$",
            ])),
            repository.read_git_command(args([
                "for-each-ref",
                "--format=%(push:remotename)",
                &format!("refs/heads/{local_branch}"),
            ])),
        )
    });
    let registry = GitHostingProviderRegistry::default_global(cx);
    cx.background_spawn(async move {
        // Remote projects cannot run git here; their origin and upstream URLs are known.
        let mut remotes = match config.await {
            Ok(Ok(output)) => parse_remote_config(&output),
            _ => Vec::new(),
        };
        if remotes.is_empty() {
            for (name, url) in [("origin", origin_url), ("upstream", upstream_url)] {
                if let Some(url) = url {
                    remotes.push(RemoteConfig {
                        name: name.into(),
                        url: Some(url),
                        gh_resolved: None,
                    });
                }
            }
        }
        let push = match pushed {
            Some(pushed) => pushed,
            None => PushTarget {
                remote: match push.await {
                    Ok(Ok(output)) => parse_push_remote(&output),
                    _ => None,
                }
                .or(upstream_remote),
                branch: Some(remote_branch),
            },
        };
        resolve_from_remotes(&remotes, &registry, local_branch, &push)
    })
}

/// What the shown status belongs to; any change asks GitHub again.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusKey {
    repository: RepositoryId,
    work_directory: Arc<Path>,
    branch: Option<git::repository::Branch>,
    head: Option<SharedString>,
    origin_url: Option<String>,
    upstream_url: Option<String>,
}

impl StatusKey {
    fn of(repository: &Repository) -> Self {
        Self {
            repository: repository.id,
            work_directory: repository.work_directory_abs_path.clone(),
            branch: repository.branch.clone(),
            head: repository
                .head_commit
                .as_ref()
                .map(|commit| commit.sha.clone()),
            origin_url: repository.remote_origin_url.clone(),
            upstream_url: repository.remote_upstream_url.clone(),
        }
    }
}

enum Outcome {
    NotGitHub,
    Fetched {
        branch: ResolvedBranch,
        result: Result<BranchPullRequests, GitHubError>,
        /// The open pull request's review threads.
        review: Option<Result<evb_github::PullRequestReview, GitHubError>>,
    },
}

fn failure_delay(error: &GitHubError, failures: u32, now: SystemTime) -> Duration {
    match error {
        GitHubError::RateLimited { retry_at, .. } => retry_at
            .and_then(|retry_at| retry_at.duration_since(now).ok())
            .unwrap_or_default()
            .max(MINIMUM_RATE_LIMIT_DELAY),
        error if error.is_missing_credential() => MISSING_CREDENTIAL_INTERVAL,
        _ => {
            let exponent = failures.saturating_sub(1).min(16);
            FAILURE_BASE_DELAY
                .saturating_mul(1 << exponent)
                .min(FAILURE_MAX_DELAY)
                .max(OPEN_PULL_REQUEST_INTERVAL)
        }
    }
}

fn success_delay(result: &BranchPullRequests) -> Duration {
    match &result.pull_request {
        Some(pull_request) if pull_request.state.is_open() && pull_request.has_pending_checks() => {
            PENDING_CHECKS_INTERVAL
        }
        Some(pull_request) if pull_request.state.is_open() => OPEN_PULL_REQUEST_INTERVAL,
        _ => IDLE_INTERVAL,
    }
}

/// The status bar button for the current branch's pull request, like JetBrains' GitHub widget.
pub struct PullRequestIndicator {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    key: Option<StatusKey>,
    pull_request: Option<PullRequest>,
    error: Option<GitHubError>,
    failures: u32,
    enabled: bool,
    task: Option<Task<()>>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    _subscriptions: Vec<Subscription>,
}

impl PullRequestIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let git_store = workspace.project().read(cx).git_store().clone();
        let subscriptions = vec![
            cx.subscribe(&git_store, |this, _, event, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_) => this.refresh(cx),
                GitStoreEvent::RepositoryUpdated(_, _, true) => this.refresh_if_changed(cx),
                _ => {}
            }),
            cx.observe_global::<SettingsStore>(|this, cx| {
                let enabled = StatusBarSettings::get_global(cx).pull_request_button;
                if enabled != this.enabled {
                    this.enabled = enabled;
                    this.refresh(cx);
                }
            }),
        ];
        let mut indicator = Self {
            workspace: workspace.weak_handle(),
            git_store,
            key: None,
            pull_request: None,
            error: None,
            failures: 0,
            enabled: StatusBarSettings::get_global(cx).pull_request_button,
            task: None,
            menu_handle: PopoverMenuHandle::default(),
            _subscriptions: subscriptions,
        };
        indicator.refresh(cx);
        indicator
    }

    fn active_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.git_store.read(cx).active_repository()
    }

    fn refresh_if_changed(&mut self, cx: &mut Context<Self>) {
        let key = self
            .active_repository(cx)
            .map(|repository| StatusKey::of(repository.read(cx)));
        if key != self.key {
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let repository = self.active_repository(cx).filter(|_| self.enabled);
        let key = repository
            .as_ref()
            .map(|repository| StatusKey::of(repository.read(cx)));
        // Another branch or repository: what was shown belongs to the old one, and its review
        // must not take writes meant for the new one.
        let identity = |key: &StatusKey| {
            (
                key.repository,
                key.branch.as_ref().map(|branch| branch.name().to_string()),
            )
        };
        if key.as_ref().map(identity) != self.key.as_ref().map(identity) {
            self.pull_request = None;
            self.error = None;
            self.failures = 0;
            if let Some(old) = &self.key {
                set_branch_review(old.work_directory.clone(), None, cx);
            }
        }
        self.key = key.clone();
        cx.notify();
        let (Some(repository), Some(key)) = (repository, key) else {
            self.task = None;
            return;
        };
        let resolve = resolve_branch(&repository, None, cx);
        let client = github_client(cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let outcome = match resolve.await {
                Some(branch) => {
                    let result = client.branch_pull_request(&branch.head).await;
                    let open = result.as_ref().ok().and_then(|result| {
                        result
                            .pull_request
                            .as_ref()
                            .filter(|pull_request| pull_request.state.is_open())
                    });
                    let review = match open {
                        Some(pull_request) => Some(
                            client
                                .review_threads(&branch.head.target, pull_request.number)
                                .await,
                        ),
                        None => None,
                    };
                    Outcome::Fetched {
                        branch,
                        result,
                        review,
                    }
                }
                None => Outcome::NotGitHub,
            };
            let Ok(Some(delay)) = this.update(cx, |this, cx| this.apply(&key, outcome, cx)) else {
                return;
            };
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.refresh(cx)).log_err();
        }));
    }

    /// Shows the outcome and returns when to ask again, or `None` for a repository that is not
    /// on GitHub.
    fn apply(
        &mut self,
        key: &StatusKey,
        outcome: Outcome,
        cx: &mut Context<Self>,
    ) -> Option<Duration> {
        if self.key.as_ref() != Some(key) {
            return None;
        }
        cx.notify();
        let (branch, result, review) = match outcome {
            Outcome::NotGitHub => {
                self.pull_request = None;
                self.error = None;
                set_branch_review(key.work_directory.clone(), None, cx);
                return None;
            }
            Outcome::Fetched {
                branch,
                result,
                review,
            } => (branch, result, review),
        };
        match result {
            Ok(result) => {
                self.failures = 0;
                self.error = None;
                self.pull_request = result.pull_request.clone();
                let delay = success_delay(&result);
                match (review, &result.pull_request) {
                    (Some(Ok(review)), Some(pull_request)) => set_branch_review(
                        key.work_directory.clone(),
                        Some(BranchReview {
                            work_directory: key.work_directory.clone(),
                            branch: branch.local_branch.clone(),
                            repository: branch.head.target.clone(),
                            url: pull_request.url.clone(),
                            review,
                        }),
                        cx,
                    ),
                    // Threads that failed to load keep showing as they were.
                    (Some(Err(error)), _) => {
                        log::warn!("could not read the pull request's review threads: {error}")
                    }
                    (None, _) => set_branch_review(key.work_directory.clone(), None, cx),
                    (Some(Ok(_)), None) => {}
                }
                cx.default_global::<PullRequestCache>()
                    .0
                    .insert((key.work_directory.clone(), branch.local_branch), result);
                Some(delay)
            }
            Err(error) => {
                self.failures += 1;
                if error.is_missing_credential() {
                    self.pull_request = None;
                } else {
                    log::warn!("could not read the pull request of the current branch: {error}");
                }
                let delay = failure_delay(&error, self.failures, SystemTime::now());
                self.error = Some(error);
                Some(delay)
            }
        }
    }

    fn build_menu(
        pull_request: PullRequest,
        review: Option<(Arc<Path>, Arc<BranchReview>)>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            let url = pull_request.url.clone();
            menu = menu
                .header(format!("#{} {}", pull_request.number, pull_request.title))
                .entry("Open on GitHub", None, {
                    let url = url.clone();
                    move |_, cx| cx.open_url(&url)
                })
                .entry("Copy Link", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                });
            if !pull_request.checks.is_empty() {
                menu = menu.separator().header("Checks");
                let mut checks = pull_request.checks.clone();
                checks.sort_by_key(|check| check_order(check.status));
                for check in checks.iter().take(MENU_CHECK_LIMIT) {
                    let (icon, color) = check_icon(check.status);
                    let entry = ui::ContextMenuEntry::new(check.name.clone())
                        .icon(icon)
                        .icon_color(color);
                    menu = match check.url.clone() {
                        Some(url) => menu.item(entry.handler(move |_, cx| cx.open_url(&url))),
                        None => menu.item(entry.disabled(true)),
                    };
                }
                if checks.len() > MENU_CHECK_LIMIT {
                    let url = format!("{}/checks", pull_request.url);
                    menu = menu.entry(
                        format!("All {} Checks…", checks.len()),
                        None,
                        move |_, cx| cx.open_url(&url),
                    );
                }
            }
            if let Some((work_directory, review)) = review.clone() {
                let mut threads = review.review.threads.clone();
                let unresolved = threads.iter().filter(|thread| !thread.is_resolved).count();
                threads.sort_by_key(|thread| thread.is_resolved);
                menu = menu.separator().header(if unresolved == 0 {
                    format!("Review Threads ({})", threads.len())
                } else {
                    format!("Review Threads ({unresolved} unresolved)")
                });
                for thread in threads.into_iter().take(MENU_THREAD_LIMIT) {
                    let summary = thread
                        .comments
                        .first()
                        .map(|comment| {
                            let line = comment.body.lines().next().unwrap_or_default();
                            let line = line.chars().take(48).collect::<String>();
                            format!("{}: {line}", comment.author)
                        })
                        .unwrap_or_default();
                    let location = match thread.line {
                        Some(line) if thread.on_head => format!("{}:{line}", thread.path),
                        Some(line) => format!("{} (deleted line {line})", thread.path),
                        None => format!("{} (outdated)", thread.path),
                    };
                    let (icon, color) = if thread.is_resolved {
                        (IconName::Check, Color::Success)
                    } else {
                        (IconName::Chat, Color::Accent)
                    };
                    let workspace = workspace.clone();
                    let work_directory = work_directory.clone();
                    menu = menu.item(
                        ui::ContextMenuEntry::new(format!("{location}  {summary}"))
                            .icon(icon)
                            .icon_color(color)
                            .handler(move |window, cx| {
                                // Deleted and outdated lines are not in the file; GitHub shows them.
                                if thread.line.is_some() && thread.on_head {
                                    crate::pull_request_review::open_thread(
                                        workspace.clone(),
                                        &work_directory,
                                        &thread,
                                        window,
                                        cx,
                                    );
                                } else if let Some(url) = thread.url() {
                                    cx.open_url(url);
                                }
                            }),
                    );
                }
                menu = menu.entry("Submit Review…", None, move |window, cx| {
                    crate::pull_request_review::submit_review(
                        workspace.clone(),
                        work_directory.clone(),
                        window,
                        cx,
                    );
                });
            }
            menu.separator()
                .action("Refresh", RefreshPullRequestStatus.boxed_clone())
        })
    }
}

fn check_order(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Failure | CheckStatus::Cancelled | CheckStatus::ActionRequired => 0,
        CheckStatus::Pending => 1,
        _ => 2,
    }
}

fn check_icon(status: CheckStatus) -> (IconName, Color) {
    match status {
        CheckStatus::Success => (IconName::Check, Color::Success),
        CheckStatus::Pending => (IconName::Circle, Color::Warning),
        CheckStatus::Failure | CheckStatus::Cancelled | CheckStatus::ActionRequired => {
            (IconName::XCircle, Color::Error)
        }
        CheckStatus::Skipped | CheckStatus::Neutral => (IconName::Dash, Color::Muted),
    }
}

fn summary_icon(summary: ChecksSummary) -> Option<(IconName, Color)> {
    match summary {
        ChecksSummary::None => None,
        ChecksSummary::Pending => Some((IconName::Circle, Color::Warning)),
        ChecksSummary::Passed => Some((IconName::Check, Color::Success)),
        ChecksSummary::Failed => Some((IconName::XCircle, Color::Error)),
    }
}

/// "3 passed, 1 failing, 2 running" and the review and merge state, for the tooltip.
pub(crate) fn describe(pull_request: &PullRequest) -> String {
    let mut parts = Vec::new();
    let count = |predicate: &dyn Fn(CheckStatus) -> bool| {
        pull_request
            .checks
            .iter()
            .filter(|check| predicate(check.status))
            .count()
    };
    let passed = count(&|status| status == CheckStatus::Success);
    let failing = count(&|status: CheckStatus| status.is_failed());
    let running = count(&|status| status == CheckStatus::Pending);
    let checks = [
        (failing, "failing"),
        (running, "running"),
        (passed, "passed"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>();
    if !checks.is_empty() {
        parts.push(format!("Checks: {}", checks.join(", ")));
    }
    match pull_request.review {
        Some(ReviewDecision::Approved) => parts.push("Approved".into()),
        Some(ReviewDecision::ChangesRequested) => parts.push("Changes requested".into()),
        Some(ReviewDecision::ReviewRequired) => parts.push("Review required".into()),
        None => {}
    }
    if pull_request.mergeable == Mergeable::Conflicting {
        parts.push(format!("Conflicts with {}", pull_request.base_branch));
    }
    parts.join(" · ")
}

impl Render for PullRequestIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.enabled {
            return gpui::Empty.into_any_element();
        }
        let Some(pull_request) = self.pull_request.clone() else {
            // A refused credential needs the user; other failures retry on their own.
            return match &self.error {
                Some(error @ GitHubError::Authentication { .. }) => {
                    let message: SharedString = error.to_string().into();
                    IconButton::new("pull-request-error", IconName::PullRequest)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Warning)
                        .tooltip(Tooltip::text(message))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                        .into_any_element()
                }
                _ => gpui::Empty.into_any_element(),
            };
        };
        let (state_label, label_color) = match pull_request.state {
            PullRequestState::Open => (None, Color::Default),
            PullRequestState::Draft => (Some("Draft"), Color::Muted),
            PullRequestState::Merged => (Some("Merged"), Color::Muted),
            PullRequestState::Closed => (Some("Closed"), Color::Muted),
        };
        let checks = pull_request
            .state
            .is_open()
            .then(|| summary_icon(pull_request.checks_summary()))
            .flatten();
        let title: SharedString =
            format!("#{} {}", pull_request.number, pull_request.title).into();
        let details: SharedString = describe(&pull_request).into();
        let trigger = ButtonLike::new("pull-request-indicator")
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("#{}", pull_request.number))
                            .size(LabelSize::Small)
                            .color(label_color),
                    )
                    .when_some(state_label, |row, state| {
                        row.child(
                            Label::new(state)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(checks, |row, (icon, color)| {
                        row.child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    })
                    .when(pull_request.mergeable == Mergeable::Conflicting, |row| {
                        row.child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                    }),
            );
        PopoverMenu::new("pull-request-menu")
            .trigger_with_tooltip(trigger, move |_, cx| {
                if details.is_empty() {
                    Tooltip::simple(title.clone(), cx)
                } else {
                    Tooltip::with_meta(title.clone(), None, details.clone(), cx)
                }
            })
            .anchor(Anchor::BottomLeft)
            .with_handle(self.menu_handle.clone())
            .menu({
                let workspace = self.workspace.clone();
                let work_directory = self.key.as_ref().map(|key| key.work_directory.clone());
                move |window, cx| {
                    let review = work_directory.clone().and_then(|work_directory| {
                        branch_review(&work_directory, cx).map(|review| (work_directory, review))
                    });
                    Some(Self::build_menu(
                        pull_request.clone(),
                        review,
                        workspace.clone(),
                        window,
                        cx,
                    ))
                }
            })
            .into_any_element()
    }
}

impl StatusItemView for PullRequestIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings
                .status_bar
                .get_or_insert_default()
                .pull_request_button = Some(false);
        }))
    }
}

/// Opens the form for a pull request of the current branch, or the hosting provider's page when
/// the repository is not on GitHub. `pushed` is the branch a push just sent, so the form is for
/// it and its destination even if the active repository changed meanwhile.
pub(crate) fn create_pull_request(
    workspace: &mut Workspace,
    pushed: Option<PushedBranch>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let repository = match &pushed {
        Some(pushed) => Some(pushed.repository.clone()),
        None => workspace
            .project()
            .read(cx)
            .git_store()
            .read(cx)
            .active_repository(),
    };
    let Some(repository) = repository else {
        return;
    };
    if let Some(PushedBranch { branch, .. }) = &pushed {
        let current = repository
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string());
        if current.as_deref() != Some(branch.as_str()) {
            let message = format!(
                "Pushed {branch}, but another branch is checked out now; create its pull request from that branch."
            );
            let toast = StatusToast::new(message, cx, |toast, _| toast.dismiss_button(true));
            workspace.toggle_status_toast(toast, cx);
            return;
        }
    }
    let push_target = pushed.map(|pushed| PushTarget {
        remote: Some(pushed.remote),
        branch: Some(pushed.remote_branch),
    });
    let resolve = resolve_branch(&repository, push_target, cx);
    let client = github_client(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let Some(branch) = resolve.await else {
            // Another host: its own page, for this repository rather than the active one.
            workspace.update_in(cx, |workspace, window, cx| {
                if let Some(panel) = workspace.panel::<crate::git_panel::GitPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.create_pull_request(Some(repository.clone()), window, cx)
                    });
                }
            })?;
            return anyhow::Ok(());
        };
        let status = match client.branch_pull_request(&branch.head).await {
            Ok(status) => status,
            Err(error) => {
                show_toast(&workspace, error.to_string(), None, cx);
                return Ok(());
            }
        };
        if let Some(open) = status
            .pull_request
            .as_ref()
            .filter(|pull_request| pull_request.state.is_open())
        {
            show_toast(
                &workspace,
                format!("Pull request #{} is already open for this branch", open.number),
                Some(open.url.clone()),
                cx,
            );
            return Ok(());
        }
        let base = status.default_branch.unwrap_or_else(|| "main".into());
        let format = "--format=%s%x00%b%x1e";
        // Without the base fetched, the branch's own commits are unknown; the last one is used.
        let mut commits = match &branch.target_remote {
            Some(remote) => {
                let range = format!("refs/remotes/{remote}/{base}..HEAD");
                read_commits(&repository, args(["log", "--reverse", format, &range]), cx).await
            }
            None => None,
        };
        if commits.is_none() {
            commits = read_commits(&repository, args(["log", "-1", format, "HEAD"]), cx).await;
        }
        let commits = commits.unwrap_or_default();
        let (title, body) = evb_github::fill_pull_request_text(&branch.local_branch, &commits);
        workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                CreatePullRequestModal::new(
                    workspace_handle,
                    client,
                    branch,
                    base,
                    title,
                    body,
                    window,
                    cx,
                )
            });
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

async fn read_commits(
    repository: &Entity<Repository>,
    arguments: Vec<String>,
    cx: &mut AsyncWindowContext,
) -> Option<Vec<(String, String)>> {
    let output = repository
        .update(cx, |repository, _| repository.read_git_command(arguments))
        .await
        .ok()?
        .ok()?;
    evb_github::parse_commit_list(&output).log_err()
}

fn show_toast(
    workspace: &WeakEntity<Workspace>,
    message: String,
    url: Option<String>,
    cx: &mut AsyncWindowContext,
) {
    workspace
        .update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, move |toast, _| {
                match url {
                    Some(url) => toast.action("Open", move |_, cx| cx.open_url(&url)),
                    None => toast,
                }
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        })
        .log_err();
}

pub(crate) struct CreatePullRequestModal {
    workspace: WeakEntity<Workspace>,
    client: Arc<GitHubClient>,
    branch: ResolvedBranch,
    base: Entity<Editor>,
    title: Entity<Editor>,
    body: Entity<Editor>,
    task: Option<Task<()>>,
}

impl CreatePullRequestModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        client: Arc<GitHubClient>,
        branch: ResolvedBranch,
        base: String,
        title: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let base = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(base, window, cx);
            editor
        });
        let title = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Title", window, cx);
            editor.set_text(title, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        let body = cx.new(|cx| {
            let mut editor = Editor::auto_height(6, 18, window, cx);
            editor.set_placeholder_text("Description", window, cx);
            editor.set_text(body, window, cx);
            editor
        });
        Self {
            workspace,
            client,
            branch,
            base,
            title,
            body,
            task: None,
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.create(false, window, cx);
    }

    fn create(&mut self, draft: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.task.is_some() {
            return;
        }
        let request = NewPullRequest {
            head: self.branch.head.clone(),
            base: self.base.read(cx).text(cx).trim().to_string(),
            title: self.title.read(cx).text(cx).trim().to_string(),
            body: self.body.read(cx).text(cx).trim_end().to_string(),
            draft,
        };
        if request.title.is_empty() || request.base.is_empty() {
            return;
        }
        let client = self.client.clone();
        let workspace = self.workspace.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            match client.create_pull_request(&request).await {
                Ok(created) => {
                    show_toast(
                        &workspace,
                        format!("Created pull request #{}", created.number),
                        Some(created.html_url),
                        cx,
                    );
                    cx.update(|window, cx| {
                        window.dispatch_action(RefreshPullRequestStatus.boxed_clone(), cx)
                    })
                    .log_err();
                    this.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
                }
                Err(error) => {
                    // Keep the form, so its text survives a fixable failure.
                    show_toast(&workspace, error.to_string(), None, cx);
                    this.update(cx, |this, cx| {
                        this.task = None;
                        cx.notify();
                    })
                    .log_err();
                }
            }
        }));
        cx.notify();
    }

    fn field(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .p_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(editor.clone()),
            )
    }
}

impl EventEmitter<DismissEvent> for CreatePullRequestModal {}
impl ModalView for CreatePullRequestModal {}
impl Focusable for CreatePullRequestModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl Render for CreatePullRequestModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = &self.branch.head.target;
        let creating = self.task.is_some();
        v_flex()
            .key_context("CreatePullRequestModal")
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
                            .child(Icon::new(IconName::PullRequest).size(IconSize::XSmall))
                            .child(
                                Headline::new("Create Pull Request").size(HeadlineSize::XSmall),
                            ),
                    )
                    .child(
                        Label::new(format!(
                            "{} into {}/{}",
                            self.branch.head.selector(),
                            target.owner,
                            target.name
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .gap_2()
                    .child(Self::field("Base branch", &self.base, cx))
                    .child(Self::field("Title", &self.title, cx))
                    .child(Self::field("Description", &self.body, cx)),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_end()
                    .gap_1()
                    .child(Button::new("cancel-pull-request", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("create-draft-pull-request", "Create Draft")
                            .disabled(creating)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("create-pull-request", "Create Pull Request")
                            .style(ButtonStyle::Filled)
                            .disabled(creating)
                            .key_binding(KeyBinding::for_action_in(
                                &Confirm,
                                &self.body.focus_handle(cx),
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
    use crate::commit_operations::tests::{
        commit_file, create_repository, init_test, open, run_git, wait_for,
    };
    use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use util::test::TempTree;

    pub(crate) struct FixedToken;

    impl evb_github::CredentialSource for FixedToken {
        fn token(&self, _host: &str) -> BoxFuture<'static, Result<String, GitHubError>> {
            futures::future::ready(Ok("secret".to_string())).boxed()
        }

        fn invalidate(&self, _host: &str) {}
    }

    pub(crate) fn json_response(status: u16, body: Value) -> Response<AsyncBody> {
        Response::builder()
            .status(status)
            .body(AsyncBody::from(body.to_string()))
            .expect("a valid response")
    }

    /// A branch on GitHub: the indicator shows its pull request and failing check, and once no
    /// pull request is open, the form fills from the branch's commits and creates one.
    #[gpui::test]
    async fn test_branch_pull_request_status_and_creation(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let tree = TempTree::new(json!({}));
        let root = tree.path();
        create_repository(root);
        let branch = "evb/pull-request-status";
        run_git(
            root,
            &["remote", "add", "origin", "https://github.com/evb0110/evb-ide.git"],
        );
        run_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run_git(root, &["switch", "-q", "-c", branch]);
        commit_file(root, "d.txt", "1\n", "Add D");
        std::fs::write(root.join("e.txt"), "1\n").expect("writing a fixture file");
        run_git(root, &["add", "e.txt"]);
        run_git(root, &["commit", "-q", "-m", "Add E", "-m", "Because E."]);
        run_git(
            root,
            &["update-ref", &format!("refs/remotes/origin/{branch}"), "HEAD"],
        );
        run_git(root, &["config", &format!("branch.{branch}.remote"), "origin"]);
        run_git(
            root,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        );

        let has_pull_request = Arc::new(AtomicBool::new(true));
        let created = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let http = FakeHttpClient::create({
            let has_pull_request = has_pull_request.clone();
            let created = created.clone();
            move |mut request| {
                let has_pull_request = has_pull_request.load(Ordering::SeqCst);
                let created = created.clone();
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
                        // The exact head: the branch on its push remote's owner.
                        assert!(uri.ends_with("&head=evb0110:evb%2Fpull-request-status"), "{uri}");
                        return Ok(json_response(
                            200,
                            if has_pull_request {
                                json!([{
                                    "number": 12,
                                    "title": "Status",
                                    "html_url": "https://github.com/evb0110/evb-ide/pull/12",
                                    "state": "open",
                                    "draft": false,
                                    "merged_at": null,
                                    "base": { "ref": "main" },
                                    "head": { "sha": "abc", "repo": { "full_name": "evb0110/evb-ide" } }
                                }])
                            } else {
                                json!([])
                            },
                        ));
                    }
                    if uri.ends_with("/pulls") {
                        created.lock().expect("recording a request").push((uri, body));
                        return Ok(json_response(
                            201,
                            json!({ "number": 13, "html_url": "https://github.com/evb0110/evb-ide/pull/13" }),
                        ));
                    }
                    let query = body["query"].as_str().unwrap_or_default();
                    if query.contains("ReviewThreads") {
                        return Ok(json_response(
                            200,
                            json!({ "data": { "repository": { "pullRequest": {
                                "id": "PR_12", "headRefOid": "abc",
                                "reviewThreads": { "pageInfo": null, "nodes": [] }
                            } } } }),
                        ));
                    }
                    if query.contains("PullRequestStatus") {
                        return Ok(json_response(
                            200,
                            json!({ "data": { "repository": { "pullRequest": {
                                "mergeable": "MERGEABLE",
                                "reviewDecision": null,
                                "commits": { "nodes": [{ "commit": { "statusCheckRollup": {
                                    "contexts": { "nodes": [
                                        { "name": "test", "status": "COMPLETED", "conclusion": "FAILURE" }
                                    ] }
                                } } }] }
                            } } } }),
                        ));
                    }
                    panic!("unexpected request {uri}")
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

        let (mut cx, repository, workspace) = open(root, cx).await;
        let workspace = workspace.upgrade().expect("the workspace is open");
        let indicator = cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                cx.new(|cx| PullRequestIndicator::new(workspace, cx))
            })
        });
        let pull_request = wait_for(&mut cx, |cx| indicator.read(cx).pull_request.clone());
        assert_eq!(pull_request.number, 12);
        assert_eq!(pull_request.checks_summary(), ChecksSummary::Failed);
        assert_eq!(describe(&pull_request), "Checks: 1 failing");
        let known = cx.update(|_, cx| known_branch(repository.read(cx), cx));
        assert_eq!(
            known.map(|known| (known.open_pull_request, known.default_branch)),
            Some((Some(12), Some("main".to_string())))
        );

        has_pull_request.store(false, Ordering::SeqCst);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| create_pull_request(workspace, None, window, cx))
        });
        let modal = wait_for(&mut cx, |cx| {
            workspace
                .read(cx)
                .active_modal::<CreatePullRequestModal>(cx)
        });
        let fields = cx.update(|_, cx| {
            let modal = modal.read(cx);
            [&modal.base, &modal.title, &modal.body].map(|editor| editor.read(cx).text(cx))
        });
        assert_eq!(
            fields,
            [
                "main".to_string(),
                "Pull request status".to_string(),
                "- Add D\n- Add E".to_string()
            ]
        );
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.title.update(cx, |title, cx| title.set_text("Show the pull request", window, cx));
            modal.create(true, window, cx);
        });
        let request = wait_for(&mut cx, |_| created.lock().expect("reading requests").first().cloned());
        assert_eq!(
            request,
            (
                "https://api.github.com/repos/evb0110/evb-ide/pulls".to_string(),
                json!({
                    "base": "main",
                    "head": "evb/pull-request-status",
                    "title": "Show the pull request",
                    "body": "- Add D\n- Add E",
                    "draft": true,
                    "maintainer_can_modify": true,
                })
            )
        );
        wait_for(&mut cx, |cx| {
            workspace
                .read(cx)
                .active_modal::<CreatePullRequestModal>(cx)
                .is_none()
                .then_some(())
        });
    }

    fn registry() -> Arc<GitHostingProviderRegistry> {
        let registry = GitHostingProviderRegistry::new();
        registry.register_hosting_provider(Arc::new(
            git_hosting_providers::Github::public_instance(),
        ));
        Arc::new(registry)
    }

    fn remote(name: &str, url: &str) -> RemoteConfig {
        RemoteConfig {
            name: name.into(),
            url: Some(url.into()),
            gh_resolved: None,
        }
    }

    fn push(remote: Option<&str>, branch: Option<&str>) -> PushTarget {
        PushTarget {
            remote: remote.map(Into::into),
            branch: branch.map(Into::into),
        }
    }

    #[test]
    fn parses_remote_config_and_push_targets() {
        assert_eq!(
            parse_remote_config(
                "remote.origin.url git@github.com:me/evb-ide.git\n\
                 remote.my.fork.url https://github.com/me/fork.git\n\
                 remote.upstream.url https://github.com/evb0110/evb-ide.git\n\
                 remote.upstream.gh-resolved base\n\
                 remote.origin.fetch +refs/heads/*:refs/remotes/origin/*\n"
            ),
            vec![
                remote("origin", "git@github.com:me/evb-ide.git"),
                remote("my.fork", "https://github.com/me/fork.git"),
                RemoteConfig {
                    gh_resolved: Some("base".into()),
                    ..remote("upstream", "https://github.com/evb0110/evb-ide.git")
                },
            ]
        );
        assert_eq!(parse_push_remote("fork\n"), Some("fork".to_string()));
        assert_eq!(parse_push_remote("\n"), None);
    }

    #[test]
    fn resolves_branches_like_gh() {
        let registry = registry();
        let locator = |owner: &str| RepositoryLocator {
            host: "github.com".into(),
            owner: owner.into(),
            name: "evb-ide".into(),
        };

        // A fork: pull requests go to upstream, the head is the pushed branch on origin.
        let fork = [
            remote("origin", "git@github.com:me/evb-ide.git"),
            remote("upstream", "https://github.com/evb0110/evb-ide.git"),
        ];
        let resolved = resolve_from_remotes(
            &fork,
            &registry,
            "local-name".into(),
            &push(Some("origin"), Some("feature")),
        )
        .unwrap();
        assert_eq!(resolved.head.target, locator("evb0110"));
        assert_eq!(resolved.head.head, locator("me"));
        assert_eq!(resolved.head.selector(), "me:feature");
        assert_eq!(resolved.target_remote.as_deref(), Some("upstream"));

        // A branch tracking upstream but pushed to a fork (`pushRemote`) has its head there.
        let tracking_upstream = resolve_from_remotes(
            &fork,
            &registry,
            "feature".into(),
            &push(Some("origin"), None),
        )
        .unwrap();
        assert_eq!(tracking_upstream.head.selector(), "me:feature");

        // `gh repo set-default` picks the base: on a remote ...
        let set_default = [
            RemoteConfig {
                gh_resolved: Some("base".into()),
                ..remote("origin", "git@github.com:me/evb-ide.git")
            },
            remote("upstream", "https://github.com/evb0110/evb-ide.git"),
        ];
        let resolved =
            resolve_from_remotes(&set_default, &registry, "feature".into(), &push(None, None))
                .unwrap();
        assert_eq!(resolved.head.target, locator("me"));
        assert_eq!(resolved.head.selector(), "feature");

        // ... or by name, when no remote points at the chosen repository.
        let by_name = [RemoteConfig {
            gh_resolved: Some("org/project".into()),
            ..remote("origin", "git@github.com:me/project.git")
        }];
        let resolved =
            resolve_from_remotes(&by_name, &registry, "feature".into(), &push(None, None))
                .unwrap();
        assert_eq!(
            resolved.head.target,
            RepositoryLocator {
                host: "github.com".into(),
                owner: "org".into(),
                name: "project".into(),
            }
        );
        assert_eq!(resolved.target_remote, None);
        assert_eq!(resolved.head.selector(), "me:feature");

        // An unpublished branch is assumed to go to origin under its own name.
        let single = [remote("origin", "https://github.com/evb0110/evb-ide")];
        let resolved =
            resolve_from_remotes(&single, &registry, "feature".into(), &push(None, None)).unwrap();
        assert!(!resolved.head.is_cross_repository());
        assert_eq!(resolved.head.branch, "feature");

        let gitlab = [remote("origin", "https://gitlab.com/evb0110/evb-ide.git")];
        assert_eq!(
            resolve_from_remotes(&gitlab, &registry, "feature".into(), &push(None, None)),
            None
        );
    }

    #[test]
    fn delays_follow_the_status() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let other = GitHubError::Request {
            host: "github.com".into(),
            detail: String::new(),
        };
        assert_eq!(failure_delay(&other, 1, now), OPEN_PULL_REQUEST_INTERVAL);
        assert_eq!(failure_delay(&other, 4, now), Duration::from_secs(240));
        assert_eq!(failure_delay(&other, 40, now), FAILURE_MAX_DELAY);
        let limited = GitHubError::RateLimited {
            host: "github.com".into(),
            retry_at: Some(now + Duration::from_secs(600)),
        };
        assert_eq!(failure_delay(&limited, 1, now), Duration::from_secs(600));
        let soon = GitHubError::RateLimited {
            host: "github.com".into(),
            retry_at: Some(now + Duration::from_secs(5)),
        };
        assert_eq!(failure_delay(&soon, 1, now), MINIMUM_RATE_LIMIT_DELAY);
        let missing = GitHubError::NotSignedIn {
            host: "github.com".into(),
        };
        assert_eq!(failure_delay(&missing, 3, now), MISSING_CREDENTIAL_INTERVAL);
    }
}

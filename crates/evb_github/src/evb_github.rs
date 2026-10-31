//! A small GitHub client for the pull request of the current branch.
//!
//! The credential lookup, request policy, branch-to-pull-request matching and check rollup follow
//! T3 Code's GitHub source control provider (MIT, see LICENSE-T3CODE).

use anyhow::Context as _;
use collections::HashMap;
use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
use gpui::BackgroundExecutor;
use http_client::{AsyncBody, HttpClient, Method, Request, http::HeaderMap};
use parking_lot::Mutex;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

/// GitHub's secondary limits penalize concurrent bursts.
const CONCURRENCY: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const MISSING_TOKEN_TTL: Duration = Duration::from_secs(10);
const GH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long requests to a host pause after a rate limit that names no reset time.
const DEFAULT_RATE_LIMIT_PAUSE: Duration = Duration::from_secs(60);
const SAME_REPOSITORY_HEAD_LIMIT: usize = 20;
/// A head owned by a fork can share its branch name with many other forks' pull requests.
const CROSS_REPOSITORY_HEAD_LIMIT: usize = 100;
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "evb-ide";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitHubError {
    CliMissing {
        host: String,
    },
    NotSignedIn {
        host: String,
    },
    CliFailed {
        host: String,
        detail: String,
    },
    Request {
        host: String,
        detail: String,
    },
    Authentication {
        host: String,
    },
    RateLimited {
        host: String,
        retry_at: Option<SystemTime>,
    },
    NotFound {
        host: String,
    },
    Response {
        host: String,
        status: u16,
        messages: Vec<String>,
    },
}

impl GitHubError {
    /// Whether the error means no credential is set up, rather than a failed request.
    pub fn is_missing_credential(&self) -> bool {
        matches!(self, Self::CliMissing { .. } | Self::NotSignedIn { .. })
    }
}

impl fmt::Display for GitHubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CliMissing { host } => write!(
                formatter,
                "No GitHub credential for {host}: set GH_TOKEN, or install the GitHub CLI and run `gh auth login`."
            ),
            Self::NotSignedIn { host } => write!(
                formatter,
                "No GitHub credential for {host}: run `gh auth login --hostname {host}`."
            ),
            Self::CliFailed { host, detail } => write!(
                formatter,
                "The GitHub CLI could not hand over a credential for {host} ({detail}). Check `gh auth status`."
            ),
            Self::Request { host, detail } => {
                write!(formatter, "Could not reach GitHub at {host}: {detail}")
            }
            Self::Authentication { host } => write!(
                formatter,
                "GitHub refused the credential for {host}. Run `gh auth login --hostname {host}` and retry."
            ),
            Self::RateLimited { .. } => write!(formatter, "GitHub API rate limit exceeded."),
            Self::NotFound { .. } => write!(
                formatter,
                "GitHub could not find the requested resource, or the credential cannot see it."
            ),
            Self::Response {
                status, messages, ..
            } => {
                if messages.is_empty() {
                    write!(formatter, "GitHub returned HTTP {status}.")
                } else {
                    write!(
                        formatter,
                        "GitHub returned an error: {}",
                        messages.join("; ")
                    )
                }
            }
        }
    }
}

impl std::error::Error for GitHubError {}

pub fn normalize_host(host: &str) -> String {
    host.trim().to_lowercase()
}

fn is_github_dot_com(host: &str) -> bool {
    host == "github.com" || host.ends_with(".ghe.com")
}

pub struct ApiUrls {
    pub rest: String,
    pub graphql: String,
}

pub fn api_urls(host: &str) -> ApiUrls {
    let host = normalize_host(host);
    if host == "github.com" {
        ApiUrls {
            rest: "https://api.github.com".into(),
            graphql: "https://api.github.com/graphql".into(),
        }
    } else if host.ends_with(".ghe.com") {
        ApiUrls {
            rest: format!("https://api.{host}"),
            graphql: format!("https://api.{host}/graphql"),
        }
    } else {
        ApiUrls {
            rest: format!("https://{host}/api/v3"),
            graphql: format!("https://{host}/api/graphql"),
        }
    }
}

/// The token from the environment, like `gh` reads it. `GH_ENTERPRISE_TOKEN` is only handed to
/// `GH_HOST`, so a repository's remote URL cannot make it leak to another host.
pub fn environment_token(host: &str, environment: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let host = normalize_host(host);
    let names: &[&str] = if is_github_dot_com(&host) {
        &["GH_TOKEN", "GITHUB_TOKEN"]
    } else if environment("GH_HOST").is_some_and(|gh_host| normalize_host(&gh_host) == host) {
        &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    } else {
        &[]
    };
    names.iter().find_map(|name| {
        environment(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

pub trait CredentialSource: Send + Sync {
    fn token(&self, host: &str) -> BoxFuture<'static, Result<String, GitHubError>>;
    fn invalidate(&self, host: &str);
}

/// Reads `GH_TOKEN` and friends, then falls back to `gh auth token`.
pub struct SystemCredentials {
    executor: BackgroundExecutor,
    cache: Arc<Mutex<HashMap<String, (Instant, Result<String, GitHubError>)>>>,
}

impl SystemCredentials {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self {
            executor,
            cache: Arc::default(),
        }
    }

    async fn from_gh(host: String, executor: BackgroundExecutor) -> Result<String, GitHubError> {
        let mut command = util::command::new_command("gh");
        command
            .args(["auth", "token", "--hostname", &host])
            // Never let gh print the token into a debug log, or wait for input.
            .env("GH_DEBUG", "")
            .env("GH_PROMPT_DISABLED", "1")
            .kill_on_drop(true);
        let output = futures::select_biased! {
            output = command.output().fuse() => output,
            _ = executor.timer(GH_TIMEOUT).fuse() => {
                return Err(GitHubError::CliFailed {
                    host,
                    detail: "`gh auth token` timed out".into(),
                });
            }
        };
        let output = match output {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(GitHubError::CliMissing { host });
            }
            Err(error) => {
                return Err(GitHubError::CliFailed {
                    host,
                    detail: error.to_string(),
                });
            }
        };
        // gh exits non-zero with "no oauth token" when it has no login for the host.
        if !output.status.success() {
            return Err(GitHubError::NotSignedIn { host });
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(GitHubError::NotSignedIn { host });
        }
        Ok(token)
    }
}

impl CredentialSource for SystemCredentials {
    fn token(&self, host: &str) -> BoxFuture<'static, Result<String, GitHubError>> {
        let host = normalize_host(host);
        if let Some((fetched_at, result)) = self.cache.lock().get(&host) {
            let ttl = if result.is_ok() {
                TOKEN_TTL
            } else {
                MISSING_TOKEN_TTL
            };
            if fetched_at.elapsed() < ttl {
                return futures::future::ready(result.clone()).boxed();
            }
        }
        let cache = self.cache.clone();
        let executor = self.executor.clone();
        async move {
            let result = match environment_token(&host, &|name| std::env::var(name).ok()) {
                Some(token) => Ok(token),
                None => Self::from_gh(host.clone(), executor).await,
            };
            cache.lock().insert(host, (Instant::now(), result.clone()));
            result
        }
        .boxed()
    }

    fn invalidate(&self, host: &str) {
        self.cache.lock().remove(&normalize_host(host));
    }
}

struct Answer {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// When a rate-limited host may be asked again: `retry-after`, else `x-ratelimit-reset`.
pub fn retry_at(headers: &HeaderMap, now: SystemTime) -> Option<SystemTime> {
    if let Some(seconds) = header(headers, "retry-after").and_then(|value| value.trim().parse::<u64>().ok())
    {
        return Some(now + Duration::from_secs(seconds));
    }
    let reset = header(headers, "x-ratelimit-reset")?.trim().parse::<u64>().ok()?;
    let reset = SystemTime::UNIX_EPOCH + Duration::from_secs(reset);
    (reset > now).then_some(reset)
}

#[derive(Debug, Default, Deserialize)]
struct GraphQlError {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// GraphQL reports rate limits with HTTP 200 and a `RATE_LIMITED` error; REST uses 403 or 429.
fn is_rate_limited(
    status: u16,
    headers: &HeaderMap,
    graphql_errors: Option<&[GraphQlError]>,
    body: &str,
) -> bool {
    let exhausted = header(headers, "x-ratelimit-remaining") == Some("0");
    status == 429
        || graphql_errors.is_some_and(|errors| {
            exhausted
                || errors.iter().any(|error| {
                    error.kind.as_deref() == Some("RATE_LIMITED")
                        || error.message.as_deref().is_some_and(|message| {
                            let message = message.to_lowercase();
                            message.contains("rate limit exceeded")
                                || message.contains("rate limit already exceeded")
                        })
                })
        })
        || (status == 403
            && (exhausted
                || header(headers, "retry-after").is_some()
                || body.to_lowercase().contains("rate limit")))
}

pub struct GitHubClient {
    http: Arc<dyn HttpClient>,
    credentials: Arc<dyn CredentialSource>,
    executor: BackgroundExecutor,
    slots: async_lock::Semaphore,
    paused_until: Mutex<HashMap<String, SystemTime>>,
}

impl GitHubClient {
    pub fn new(
        http: Arc<dyn HttpClient>,
        credentials: Arc<dyn CredentialSource>,
        executor: BackgroundExecutor,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            credentials,
            executor,
            slots: async_lock::Semaphore::new(CONCURRENCY),
            paused_until: Mutex::new(HashMap::default()),
        })
    }

    async fn send(
        &self,
        host: &str,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Answer, GitHubError> {
        let host = normalize_host(host);
        if let Some(until) = self.paused_until.lock().get(&host).copied()
            && until > SystemTime::now()
        {
            return Err(GitHubError::RateLimited {
                host,
                retry_at: Some(until),
            });
        }
        let token = self.credentials.token(&host).await?;
        let _slot = self.slots.acquire().await;
        let request = Request::builder()
            .method(method)
            .uri(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("Content-Type", "application/json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", USER_AGENT)
            .body(match body {
                Some(body) => AsyncBody::from(body.to_string()),
                None => AsyncBody::default(),
            })
            .map_err(|error| GitHubError::Request {
                host: host.clone(),
                detail: error.to_string(),
            })?;
        let exchange = async {
            let mut response = self.http.send(request).await?;
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await?;
            anyhow::Ok(Answer {
                status: response.status().as_u16(),
                headers: response.headers().clone(),
                body,
            })
        };
        futures::select_biased! {
            answer = exchange.fuse() => answer.map_err(|error| GitHubError::Request {
                host: host.clone(),
                detail: format!("{error:#}"),
            }),
            _ = self.executor.timer(REQUEST_TIMEOUT).fuse() => Err(GitHubError::Request {
                host: host.clone(),
                detail: "the request timed out".into(),
            }),
        }
    }

    /// Turns HTTP and GraphQL failures into [`GitHubError`]s, pausing the host on rate limits.
    fn check_answer(
        &self,
        host: &str,
        answer: &Answer,
        graphql_errors: Option<&[GraphQlError]>,
    ) -> Result<(), GitHubError> {
        let host = normalize_host(host);
        let body = String::from_utf8_lossy(&answer.body);
        if is_rate_limited(answer.status, &answer.headers, graphql_errors, &body) {
            let now = SystemTime::now();
            let retry_at = retry_at(&answer.headers, now);
            self.paused_until
                .lock()
                .insert(host.clone(), retry_at.unwrap_or(now + DEFAULT_RATE_LIMIT_PAUSE));
            return Err(GitHubError::RateLimited { host, retry_at });
        }
        match answer.status {
            401 => {
                self.credentials.invalidate(&host);
                Err(GitHubError::Authentication { host })
            }
            404 => Err(GitHubError::NotFound { host }),
            status if !(200..300).contains(&status) => Err(GitHubError::Response {
                host,
                status,
                messages: rest_error_messages(&answer.body),
            }),
            _ => Ok(()),
        }
    }

    pub async fn graphql<T: DeserializeOwned>(
        &self,
        host: &str,
        query: &str,
        variables: Value,
    ) -> Result<T, GitHubError> {
        #[derive(Deserialize)]
        struct Envelope<T> {
            data: Option<T>,
            #[serde(default)]
            errors: Option<Vec<GraphQlError>>,
        }

        let body = json!({ "query": query, "variables": variables });
        let answer = self
            .send(host, Method::POST, &api_urls(host).graphql, Some(&body))
            .await?;
        let envelope = serde_json::from_slice::<Envelope<T>>(&answer.body);
        let graphql_errors = envelope
            .as_ref()
            .ok()
            .and_then(|envelope| envelope.errors.as_deref());
        self.check_answer(host, &answer, graphql_errors)?;
        let envelope = envelope.map_err(|error| GitHubError::Response {
            host: normalize_host(host),
            status: answer.status,
            messages: vec![format!("unexpected response: {error}")],
        })?;
        match (envelope.data, envelope.errors) {
            (Some(data), errors) => {
                if let Some(errors) = errors.filter(|errors| !errors.is_empty()) {
                    log::warn!(
                        "GitHub answered with partial errors: {}",
                        errors
                            .iter()
                            .filter_map(|error| error.message.as_deref())
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                }
                Ok(data)
            }
            (None, errors) => {
                let errors = errors.unwrap_or_default();
                if errors
                    .iter()
                    .any(|error| error.kind.as_deref() == Some("NOT_FOUND"))
                {
                    return Err(GitHubError::NotFound {
                        host: normalize_host(host),
                    });
                }
                Err(GitHubError::Response {
                    host: normalize_host(host),
                    status: answer.status,
                    messages: errors.into_iter().filter_map(|error| error.message).collect(),
                })
            }
        }
    }

    pub async fn rest<T: DeserializeOwned>(
        &self,
        host: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, GitHubError> {
        let url = format!("{}/{}", api_urls(host).rest, path.trim_start_matches('/'));
        let answer = self.send(host, method, &url, body.as_ref()).await?;
        self.check_answer(host, &answer, None)?;
        serde_json::from_slice(&answer.body).map_err(|error| GitHubError::Response {
            host: normalize_host(host),
            status: answer.status,
            messages: vec![format!("unexpected response: {error}")],
        })
    }

    /// The pull request of a branch: an open one first, else the most recently updated one.
    pub async fn branch_pull_request(
        &self,
        branch: &BranchHead,
    ) -> Result<BranchPullRequests, GitHubError> {
        let target = &branch.target;
        let first = if branch.is_cross_repository() {
            CROSS_REPOSITORY_HEAD_LIMIT
        } else {
            SAME_REPOSITORY_HEAD_LIMIT
        };
        let data: BranchQueryData = self
            .graphql(
                &target.host,
                BRANCH_PULL_REQUESTS_QUERY,
                json!({
                    "owner": target.owner,
                    "name": target.name,
                    "head": branch.branch,
                    "first": first,
                }),
            )
            .await?;
        let repository = data.repository.ok_or_else(|| GitHubError::NotFound {
            host: normalize_host(&target.host),
        })?;
        let default_branch = repository.default_branch_ref.map(|branch| branch.name);
        let Some(node) = choose_pull_request(&repository.pull_requests.nodes, branch) else {
            return Ok(BranchPullRequests {
                default_branch,
                pull_request: None,
            });
        };
        let mut pull_request = PullRequest {
            number: node.number,
            title: node.title.clone(),
            url: node.url.clone(),
            state: node.state(),
            base_branch: node.base_ref_name.clone(),
            head_sha: node.head_ref_oid.clone(),
            checks: Vec::new(),
            mergeable: Mergeable::Unknown,
            review: None,
        };
        if pull_request.state.is_open() {
            let status: StatusQueryData = self
                .graphql(
                    &target.host,
                    PULL_REQUEST_STATUS_QUERY,
                    json!({
                        "owner": target.owner,
                        "name": target.name,
                        "number": node.number,
                        // An older Enterprise server may not know `isRequired`, and an
                        // unknown field fails the whole read.
                        "required": is_github_dot_com(&normalize_host(&target.host)),
                    }),
                )
                .await?;
            if let Some(status) = status.repository.and_then(|repository| repository.pull_request)
            {
                pull_request.mergeable = match status.mergeable.as_deref() {
                    Some("MERGEABLE") => Mergeable::Mergeable,
                    Some("CONFLICTING") => Mergeable::Conflicting,
                    _ => Mergeable::Unknown,
                };
                pull_request.review = status.review_decision.as_deref().and_then(|decision| {
                    match decision {
                        "APPROVED" => Some(ReviewDecision::Approved),
                        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
                        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
                        _ => None,
                    }
                });
                let contexts = status
                    .commits
                    .nodes
                    .into_iter()
                    .next()
                    .and_then(|node| node.commit.status_check_rollup)
                    .map(|rollup| rollup.contexts.nodes)
                    .unwrap_or_default();
                pull_request.checks = collapse_checks(contexts);
            }
        }
        Ok(BranchPullRequests {
            default_branch,
            pull_request: Some(pull_request),
        })
    }

    pub async fn create_pull_request(
        &self,
        request: &NewPullRequest,
    ) -> Result<CreatedPullRequest, GitHubError> {
        let target = &request.head.target;
        self.rest(
            &target.host,
            Method::POST,
            &format!("repos/{}/{}/pulls", target.owner, target.name),
            Some(json!({
                "base": request.base,
                "head": request.head.selector(),
                "title": request.title,
                "body": request.body,
                "draft": request.draft,
                // Match gh; the API default is undocumented.
                "maintainer_can_modify": true,
            })),
        )
        .await
    }
}

fn rest_error_messages(body: &[u8]) -> Vec<String> {
    #[derive(Deserialize)]
    struct RestError {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        errors: Vec<Value>,
    }
    let Ok(error) = serde_json::from_slice::<RestError>(body) else {
        return Vec::new();
    };
    let details = error.errors.iter().filter_map(|detail| match detail {
        Value::String(message) => Some(message.clone()),
        Value::Object(fields) => fields
            .get("message")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        _ => None,
    });
    error.message.into_iter().chain(details).collect()
}

/// A repository on a GitHub host.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryLocator {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepositoryLocator {
    fn same_repository(&self, other: &Self) -> bool {
        normalize_host(&self.host) == normalize_host(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }
}

/// A pushed branch and the repository its pull requests are opened in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BranchHead {
    /// The repository pull requests target, like `gh`'s base repository.
    pub target: RepositoryLocator,
    /// The repository the branch is pushed to.
    pub head: RepositoryLocator,
    /// The branch name on the head repository.
    pub branch: String,
}

impl BranchHead {
    pub fn is_cross_repository(&self) -> bool {
        !self.target.same_repository(&self.head)
    }

    /// How the head is named when creating a pull request: `owner:branch` for a fork.
    pub fn selector(&self) -> String {
        if self.is_cross_repository() {
            format!("{}:{}", self.head.owner, self.branch)
        } else {
            self.branch.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PullRequestState {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Open | Self::Draft)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pending,
    Success,
    Failure,
    Cancelled,
    ActionRequired,
    Skipped,
    Neutral,
}

impl CheckStatus {
    /// Action required is a finished check that needs a person.
    pub fn is_failed(self) -> bool {
        matches!(self, Self::Failure | Self::Cancelled | Self::ActionRequired)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub required: bool,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksSummary {
    None,
    Pending,
    Passed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    pub base_branch: String,
    pub head_sha: Option<String>,
    pub checks: Vec<Check>,
    pub mergeable: Mergeable,
    pub review: Option<ReviewDecision>,
}

impl PullRequest {
    /// Failing checks win; otherwise the required checks, or all when none is required, decide.
    pub fn checks_summary(&self) -> ChecksSummary {
        if self.checks.is_empty() {
            return ChecksSummary::None;
        }
        if self.checks.iter().any(|check| check.status.is_failed()) {
            return ChecksSummary::Failed;
        }
        let required = self.checks.iter().any(|check| check.required);
        let gate = self
            .checks
            .iter()
            .filter(|check| !required || check.required);
        if gate
            .clone()
            .all(|check| check.status != CheckStatus::Pending)
        {
            ChecksSummary::Passed
        } else {
            ChecksSummary::Pending
        }
    }

    pub fn has_pending_checks(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Pending)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchPullRequests {
    pub default_branch: Option<String>,
    pub pull_request: Option<PullRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPullRequest {
    pub head: BranchHead,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CreatedPullRequest {
    pub number: u64,
    pub html_url: String,
}

const BRANCH_PULL_REQUESTS_QUERY: &str = r#"
query BranchPullRequests($owner: String!, $name: String!, $head: String!, $first: Int!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef { name }
    pullRequests(headRefName: $head, first: $first, orderBy: { field: CREATED_AT, direction: DESC }) {
      nodes {
        number
        title
        url
        state
        isDraft
        baseRefName
        headRefOid
        updatedAt
        isCrossRepository
        headRepositoryOwner { login }
      }
    }
  }
}
"#;

const PULL_REQUEST_STATUS_QUERY: &str = r#"
query PullRequestStatus($owner: String!, $name: String!, $number: Int!, $required: Boolean!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      mergeable
      reviewDecision
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100) {
                nodes {
                  ... on StatusContext {
                    context
                    state
                    targetUrl
                    createdAt
                    isRequired(pullRequestNumber: $number) @include(if: $required)
                  }
                  ... on CheckRun {
                    name
                    status
                    conclusion
                    detailsUrl
                    startedAt
                    completedAt
                    isRequired(pullRequestNumber: $number) @include(if: $required)
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
"#;

#[derive(Deserialize)]
struct BranchQueryData {
    repository: Option<BranchQueryRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchQueryRepository {
    default_branch_ref: Option<NamedRef>,
    pull_requests: Nodes<PullRequestNode>,
}

#[derive(Deserialize)]
struct NamedRef {
    name: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    number: u64,
    title: String,
    url: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    base_ref_name: String,
    #[serde(default)]
    head_ref_oid: Option<String>,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    is_cross_repository: bool,
    #[serde(default)]
    head_repository_owner: Option<Login>,
}

#[derive(Clone, Debug, Deserialize)]
struct Login {
    login: String,
}

impl PullRequestNode {
    fn state(&self) -> PullRequestState {
        match self.state.as_str() {
            "MERGED" => PullRequestState::Merged,
            "CLOSED" => PullRequestState::Closed,
            _ if self.is_draft => PullRequestState::Draft,
            _ => PullRequestState::Open,
        }
    }

    fn matches(&self, branch: &BranchHead) -> bool {
        let owner = self
            .head_repository_owner
            .as_ref()
            .map(|owner| owner.login.as_str());
        if branch.is_cross_repository() {
            self.is_cross_repository
                && owner.is_some_and(|owner| owner.eq_ignore_ascii_case(&branch.head.owner))
        } else {
            !self.is_cross_repository
        }
    }
}

fn choose_pull_request<'a>(
    nodes: &'a [PullRequestNode],
    branch: &BranchHead,
) -> Option<&'a PullRequestNode> {
    let mut matching = nodes
        .iter()
        .filter(|node| node.matches(branch))
        .collect::<Vec<_>>();
    // ISO 8601 timestamps sort chronologically as text.
    matching.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    matching
        .iter()
        .find(|node| node.state().is_open())
        .or_else(|| matching.first())
        .copied()
}

#[derive(Deserialize)]
struct StatusQueryData {
    repository: Option<StatusQueryRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusQueryRepository {
    pull_request: Option<StatusQueryPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusQueryPullRequest {
    mergeable: Option<String>,
    review_decision: Option<String>,
    commits: Nodes<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: StatusCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusCommit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    contexts: Nodes<CheckNode>,
}

/// A `CheckRun` or a `StatusContext`; the other variant's fields stay empty.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CheckNode {
    name: Option<String>,
    context: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
    details_url: Option<String>,
    target_url: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    created_at: Option<String>,
    is_required: Option<bool>,
}

/// GitHub's sentinel for timestamps not reached yet.
const UNSET_TIMESTAMP: &str = "0001-01-01T00:00:00Z";

fn check_status(node: &CheckNode) -> CheckStatus {
    if let Some(status) = node.status.as_deref().map(str::trim)
        && !status.is_empty()
        && !status.eq_ignore_ascii_case("COMPLETED")
    {
        return CheckStatus::Pending;
    }
    match node
        .conclusion
        .as_deref()
        .or(node.state.as_deref())
        .map(|value| value.trim().to_uppercase())
        .as_deref()
    {
        Some("SUCCESS") => CheckStatus::Success,
        Some("ACTION_REQUIRED") => CheckStatus::ActionRequired,
        Some("FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE") => CheckStatus::Failure,
        Some("CANCELLED") => CheckStatus::Cancelled,
        Some("SKIPPED") => CheckStatus::Skipped,
        Some("PENDING" | "EXPECTED") => CheckStatus::Pending,
        _ => CheckStatus::Neutral,
    }
}

/// One entry per check name: a run still going outranks finished runs of the same check (a push
/// and a pull request workflow can both report it), else the latest finished run wins.
fn collapse_checks(nodes: Vec<CheckNode>) -> Vec<Check> {
    let mut latest: Vec<(Check, Option<String>)> = Vec::new();
    for node in nodes {
        let Some(name) = node
            .name
            .clone()
            .or_else(|| node.context.clone())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let status = check_status(&node);
        let at = [&node.completed_at, &node.started_at, &node.created_at]
            .into_iter()
            .flatten()
            .find(|at| at.as_str() != UNSET_TIMESTAMP)
            .cloned();
        let check = Check {
            name,
            status,
            required: node.is_required.unwrap_or(false),
            url: node.details_url.or(node.target_url),
        };
        match latest.iter_mut().find(|(existing, _)| existing.name == check.name) {
            Some((existing, existing_at)) => {
                let required = existing.required || check.required;
                let replaces = match (
                    existing.status == CheckStatus::Pending,
                    check.status == CheckStatus::Pending,
                ) {
                    (false, true) => true,
                    (true, false) => false,
                    _ => at > *existing_at,
                };
                if replaces {
                    *existing = check;
                    *existing_at = at;
                }
                existing.required = required;
            }
            None => latest.push((check, at)),
        }
    }
    latest.into_iter().map(|(check, _)| check).collect()
}

/// `gh pr create --fill`: one commit gives the title and body, several give the branch name and
/// a list of their subjects.
pub fn fill_pull_request_text(branch: &str, commits: &[(String, String)]) -> (String, String) {
    match commits {
        [(subject, body)] => (subject.trim().to_string(), body.trim().to_string()),
        _ => {
            let mut title = branch
                .rsplit('/')
                .next()
                .unwrap_or(branch)
                .replace(['-', '_'], " ");
            if let Some(first) = title.get(..1) {
                title = first.to_uppercase() + &title[1..];
            }
            let body = commits
                .iter()
                .map(|(subject, _)| format!("- {}", subject.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            (title, body)
        }
    }
}

pub fn parse_commit_list(output: &str) -> anyhow::Result<Vec<(String, String)>> {
    output
        .split('\u{1e}')
        .map(|record| record.trim_start_matches('\n'))
        .filter(|record| !record.trim().is_empty())
        .map(|record| {
            let (subject, body) = record
                .split_once('\0')
                .context("a commit record has no subject separator")?;
            Ok((subject.to_string(), body.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use pretty_assertions::assert_eq;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedToken;

    impl CredentialSource for FixedToken {
        fn token(&self, _host: &str) -> BoxFuture<'static, Result<String, GitHubError>> {
            futures::future::ready(Ok("secret".to_string())).boxed()
        }

        fn invalidate(&self, _host: &str) {}
    }

    fn locator(owner: &str, name: &str) -> RepositoryLocator {
        RepositoryLocator {
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn same_repository_branch(branch: &str) -> BranchHead {
        BranchHead {
            target: locator("evb0110", "evb-ide"),
            head: locator("evb0110", "evb-ide"),
            branch: branch.into(),
        }
    }

    fn node(number: u64, state: &str, updated_at: &str, owner: &str, cross: bool) -> Value {
        json!({
            "number": number,
            "title": format!("PR {number}"),
            "url": format!("https://github.com/evb0110/evb-ide/pull/{number}"),
            "state": state,
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": "abc",
            "updatedAt": updated_at,
            "isCrossRepository": cross,
            "headRepositoryOwner": { "login": owner },
        })
    }

    fn nodes(values: Vec<Value>) -> Vec<PullRequestNode> {
        values
            .into_iter()
            .map(|value| serde_json::from_value(value).unwrap())
            .collect()
    }

    #[test]
    fn environment_token_follows_gh() {
        let environment = |values: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        let both = environment(&[("GH_TOKEN", "gh"), ("GITHUB_TOKEN", "github")]);
        assert_eq!(environment_token("GitHub.com", &both), Some("gh".into()));
        let blank = environment(&[("GH_TOKEN", "  "), ("GITHUB_TOKEN", "github")]);
        assert_eq!(environment_token("github.com", &blank), Some("github".into()));
        let enterprise = environment(&[
            ("GH_TOKEN", "gh"),
            ("GH_HOST", "git.example.com"),
            ("GH_ENTERPRISE_TOKEN", "enterprise"),
        ]);
        assert_eq!(
            environment_token("git.example.com", &enterprise),
            Some("enterprise".into())
        );
        // A remote naming another host must not receive the enterprise token.
        assert_eq!(environment_token("evil.example.com", &enterprise), None);
        assert_eq!(environment_token("tenant.ghe.com", &both), Some("gh".into()));
    }

    #[test]
    fn api_urls_per_host() {
        assert_eq!(api_urls("github.com").graphql, "https://api.github.com/graphql");
        assert_eq!(api_urls("acme.ghe.com").rest, "https://api.acme.ghe.com");
        assert_eq!(
            api_urls("git.example.com").graphql,
            "https://git.example.com/api/graphql"
        );
    }

    #[test]
    fn rate_limit_detection() {
        let mut exhausted = HeaderMap::new();
        exhausted.insert("x-ratelimit-remaining", "0".parse().unwrap());
        let empty = HeaderMap::new();
        assert!(is_rate_limited(429, &empty, None, ""));
        assert!(is_rate_limited(403, &exhausted, None, ""));
        assert!(is_rate_limited(
            403,
            &empty,
            None,
            "You have exceeded a secondary rate limit"
        ));
        assert!(!is_rate_limited(403, &empty, None, "Resource not accessible"));
        let limited = [GraphQlError {
            kind: Some("RATE_LIMITED".into()),
            message: None,
        }];
        assert!(is_rate_limited(200, &empty, Some(&limited), ""));
        let other = [GraphQlError {
            kind: Some("NOT_FOUND".into()),
            message: Some("Could not resolve".into()),
        }];
        assert!(!is_rate_limited(200, &empty, Some(&other), ""));

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut retry = HeaderMap::new();
        retry.insert("retry-after", "30".parse().unwrap());
        assert_eq!(retry_at(&retry, now), Some(now + Duration::from_secs(30)));
        let mut reset = HeaderMap::new();
        reset.insert("x-ratelimit-reset", "1500".parse().unwrap());
        assert_eq!(
            retry_at(&reset, now),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_500))
        );
        let mut past = HeaderMap::new();
        past.insert("x-ratelimit-reset", "500".parse().unwrap());
        assert_eq!(retry_at(&past, now), None);
    }

    #[test]
    fn chooses_open_then_latest_pull_request() {
        let branch = same_repository_branch("feature");
        let list = nodes(vec![
            node(3, "CLOSED", "2026-10-30T10:00:00Z", "evb0110", false),
            node(2, "OPEN", "2026-10-01T10:00:00Z", "evb0110", false),
            node(4, "OPEN", "2026-10-31T10:00:00Z", "someone", true),
        ]);
        assert_eq!(choose_pull_request(&list, &branch).map(|node| node.number), Some(2));

        let closed = nodes(vec![
            node(5, "MERGED", "2026-10-01T10:00:00Z", "evb0110", false),
            node(6, "CLOSED", "2026-10-02T10:00:00Z", "evb0110", false),
        ]);
        assert_eq!(choose_pull_request(&closed, &branch).map(|node| node.number), Some(6));

        // From a fork, only pull requests whose head the fork owns are the branch's.
        let fork_branch = BranchHead {
            head: locator("contributor", "evb-ide"),
            ..branch
        };
        assert!(fork_branch.is_cross_repository());
        assert_eq!(fork_branch.selector(), "contributor:feature");
        let fork_list = nodes(vec![
            node(7, "OPEN", "2026-10-31T10:00:00Z", "evb0110", false),
            node(8, "OPEN", "2026-10-30T10:00:00Z", "someone", true),
            node(9, "OPEN", "2026-10-29T10:00:00Z", "Contributor", true),
        ]);
        assert_eq!(
            choose_pull_request(&fork_list, &fork_branch).map(|node| node.number),
            Some(9)
        );
    }

    fn check(value: Value) -> CheckNode {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn collapses_and_summarizes_checks() {
        let checks = collapse_checks(vec![
            check(json!({ "name": "test", "status": "COMPLETED", "conclusion": "FAILURE", "completedAt": "2026-10-31T09:00:00Z" })),
            check(json!({ "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS", "completedAt": "2026-10-31T10:00:00Z" })),
            check(json!({ "name": "lint", "status": "IN_PROGRESS", "conclusion": null, "isRequired": true })),
            check(json!({ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS", "completedAt": "2026-10-31T11:00:00Z" })),
            check(json!({ "context": "ci/legacy", "state": "SUCCESS", "targetUrl": "https://ci.example.com/1" })),
            check(json!({ "name": "  " })),
        ]);
        assert_eq!(
            checks,
            vec![
                Check {
                    name: "test".into(),
                    status: CheckStatus::Success,
                    required: false,
                    url: None,
                },
                Check {
                    name: "lint".into(),
                    status: CheckStatus::Pending,
                    required: true,
                    url: None,
                },
                Check {
                    name: "ci/legacy".into(),
                    status: CheckStatus::Success,
                    required: false,
                    url: Some("https://ci.example.com/1".into()),
                },
            ]
        );

        let pull_request = |checks: Vec<Check>| PullRequest {
            number: 1,
            title: String::new(),
            url: String::new(),
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_sha: None,
            checks,
            mergeable: Mergeable::Unknown,
            review: None,
        };
        let named = |name: &str, status: CheckStatus, required: bool| Check {
            name: name.into(),
            status,
            required,
            url: None,
        };
        assert_eq!(pull_request(checks).checks_summary(), ChecksSummary::Pending);
        assert_eq!(pull_request(Vec::new()).checks_summary(), ChecksSummary::None);
        // Only required checks gate passing; an advisory one still running does not.
        assert_eq!(
            pull_request(vec![
                named("required", CheckStatus::Success, true),
                named("advisory", CheckStatus::Pending, false),
            ])
            .checks_summary(),
            ChecksSummary::Passed
        );
        assert_eq!(
            pull_request(vec![
                named("required", CheckStatus::Success, true),
                named("advisory", CheckStatus::ActionRequired, false),
            ])
            .checks_summary(),
            ChecksSummary::Failed
        );
        assert_eq!(
            pull_request(vec![
                named("a", CheckStatus::Skipped, false),
                named("b", CheckStatus::Neutral, false),
            ])
            .checks_summary(),
            ChecksSummary::Passed
        );
    }

    #[test]
    fn fills_like_gh() {
        assert_eq!(
            fill_pull_request_text(
                "feature/x",
                &[("Fix the thing".into(), "Because.\n".into())]
            ),
            ("Fix the thing".into(), "Because.".into())
        );
        assert_eq!(
            fill_pull_request_text(
                "evb/pull-request_status",
                &[("One".into(), String::new()), ("Two".into(), String::new())]
            ),
            ("Pull request status".into(), "- One\n- Two".into())
        );
        assert_eq!(
            parse_commit_list("One\0Body\nmore\n\u{1e}\nTwo\0\n\u{1e}\n").unwrap(),
            vec![
                ("One".to_string(), "Body\nmore\n".to_string()),
                ("Two".to_string(), "\n".to_string())
            ]
        );
    }

    fn json_response(status: u16, body: Value) -> Response<AsyncBody> {
        Response::builder()
            .status(status)
            .body(AsyncBody::from(body.to_string()))
            .unwrap()
    }

    #[gpui::test]
    async fn reads_the_branch_pull_request_with_checks(cx: &mut gpui::TestAppContext) {
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = FakeHttpClient::create({
            let requests = requests.clone();
            move |mut request| {
                let requests = requests.clone();
                async move {
                    assert_eq!(request.uri().to_string(), "https://api.github.com/graphql");
                    assert_eq!(
                        request.headers().get("Authorization").unwrap(),
                        "Bearer secret"
                    );
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    let body: Value = serde_json::from_str(&body)?;
                    let is_status = body["query"]
                        .as_str()
                        .is_some_and(|query| query.contains("PullRequestStatus"));
                    requests.lock().push(body["variables"].clone());
                    Ok(if is_status {
                        json_response(
                            200,
                            json!({ "data": { "repository": { "pullRequest": {
                                "mergeable": "CONFLICTING",
                                "reviewDecision": "APPROVED",
                                "commits": { "nodes": [{ "commit": { "statusCheckRollup": {
                                    "contexts": { "nodes": [
                                        { "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://ci/1", "isRequired": true },
                                        {}
                                    ] }
                                } } }] }
                            } } } }),
                        )
                    } else {
                        json_response(
                            200,
                            json!({ "data": { "repository": {
                                "defaultBranchRef": { "name": "main" },
                                "pullRequests": { "nodes": [
                                    node(12, "OPEN", "2026-10-31T10:00:00Z", "evb0110", false)
                                ] }
                            } } }),
                        )
                    })
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let result = client
            .branch_pull_request(&same_repository_branch("feature"))
            .await
            .unwrap();
        assert_eq!(result.default_branch.as_deref(), Some("main"));
        let pull_request = result.pull_request.unwrap();
        assert_eq!(pull_request.number, 12);
        assert_eq!(pull_request.state, PullRequestState::Open);
        assert_eq!(pull_request.mergeable, Mergeable::Conflicting);
        assert_eq!(pull_request.review, Some(ReviewDecision::Approved));
        assert_eq!(pull_request.checks_summary(), ChecksSummary::Passed);
        assert_eq!(
            *requests.lock(),
            vec![
                json!({ "owner": "evb0110", "name": "evb-ide", "head": "feature", "first": 20 }),
                json!({ "owner": "evb0110", "name": "evb-ide", "number": 12, "required": true }),
            ]
        );
    }

    #[gpui::test]
    async fn maps_failures_and_pauses_after_a_rate_limit(cx: &mut gpui::TestAppContext) {
        let calls = Arc::new(AtomicUsize::new(0));
        let http = FakeHttpClient::create({
            let calls = calls.clone();
            move |_| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(match call {
                        0 => json_response(401, json!({ "message": "Bad credentials" })),
                        1 => json_response(
                            422,
                            json!({ "message": "Validation Failed", "errors": [{ "message": "A pull request already exists for evb0110:feature." }] }),
                        ),
                        _ => Response::builder()
                            .status(403)
                            .header("retry-after", "120")
                            .body(AsyncBody::from("secondary rate limit".to_string()))
                            .unwrap(),
                    })
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let request = NewPullRequest {
            head: same_repository_branch("feature"),
            base: "main".into(),
            title: "Title".into(),
            body: String::new(),
            draft: false,
        };
        assert_eq!(
            client.create_pull_request(&request).await,
            Err(GitHubError::Authentication {
                host: "github.com".into()
            })
        );
        let error = client.create_pull_request(&request).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub returned an error: Validation Failed; A pull request already exists for evb0110:feature."
        );
        assert!(matches!(
            client.create_pull_request(&request).await,
            Err(GitHubError::RateLimited {
                retry_at: Some(_),
                ..
            })
        ));
        // The host stays paused, so the next request is not sent.
        assert!(matches!(
            client.create_pull_request(&request).await,
            Err(GitHubError::RateLimited { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[gpui::test]
    async fn creates_a_fork_pull_request(cx: &mut gpui::TestAppContext) {
        let sent = Arc::new(Mutex::new(None::<(String, Value)>));
        let http = FakeHttpClient::create({
            let sent = sent.clone();
            move |mut request| {
                let sent = sent.clone();
                async move {
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    *sent.lock() = Some((request.uri().to_string(), serde_json::from_str(&body)?));
                    Ok(json_response(
                        201,
                        json!({ "number": 41, "html_url": "https://github.com/evb0110/evb-ide/pull/41" }),
                    ))
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let created = client
            .create_pull_request(&NewPullRequest {
            head: BranchHead {
                target: locator("evb0110", "evb-ide"),
                head: locator("contributor", "evb-ide"),
                branch: "feature".into(),
            },
            base: "main".into(),
            title: "Title".into(),
            body: "Body".into(),
                draft: true,
            })
            .await
            .unwrap();
        assert_eq!(created.number, 41);
        assert_eq!(
            sent.lock().clone(),
            Some((
                "https://api.github.com/repos/evb0110/evb-ide/pulls".to_string(),
                json!({
                    "base": "main",
                    "head": "contributor:feature",
                    "title": "Title",
                    "body": "Body",
                    "draft": true,
                    "maintainer_can_modify": true,
                })
            ))
        );
    }
}

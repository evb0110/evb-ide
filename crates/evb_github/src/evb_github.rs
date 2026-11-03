//! A small GitHub client for the pull request of the current branch.
//!
//! The credential lookup, request policy, branch-to-pull-request matching and check rollup follow
//! T3 Code's GitHub source control provider (MIT, see LICENSE-T3CODE).

use anyhow::Context as _;
use collections::HashMap;
use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
use gpui::BackgroundExecutor;
use http_client::{
    AsyncBody, HttpClient, HttpRequestExt as _, Method, RedirectPolicy, Request, http::HeaderMap,
};
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
const DEFAULT_BRANCH_TTL: Duration = Duration::from_secs(60 * 60);
const REQUIRED_CHECKS_TTL: Duration = Duration::from_secs(10 * 60);
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

/// A host as remotes and `gh` name it: a DNS name, optionally with a port. Anything else, such as
/// a path, query or user info from a crafted `gh-resolved`, would move the API URL, and the token
/// sent to it, to another host.
pub fn is_valid_host(host: &str) -> bool {
    let host = normalize_host(host);
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host.as_str(), None),
    };
    let valid_port = port.is_none_or(|port| {
        !port.is_empty() && port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit())
    });
    let valid_name = !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    valid_port && valid_name
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
pub fn environment_token(
    host: &str,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
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
            // gh hands out GH_ENTERPRISE_TOKEN for any host; environment_token() already
            // decided which host may have an environment token, so gh reads only its logins.
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN")
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
    if let Some(seconds) =
        header(headers, "retry-after").and_then(|value| value.trim().parse::<u64>().ok())
    {
        // An absurd value would overflow; the caller then falls back to its default pause.
        return now.checked_add(Duration::from_secs(seconds));
    }
    let reset = header(headers, "x-ratelimit-reset")?
        .trim()
        .parse::<u64>()
        .ok()?;
    let reset = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(reset))?;
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
    default_branches: Mutex<HashMap<(String, String, String), (Instant, String)>>,
    required_checks: Mutex<HashMap<(String, String, String, String), (Instant, Vec<String>)>>,
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
            default_branches: Mutex::new(HashMap::default()),
            required_checks: Mutex::new(HashMap::default()),
        })
    }

    fn check_paused(&self, host: &str) -> Result<(), GitHubError> {
        match self.paused_until.lock().get(host).copied() {
            Some(until) if until > SystemTime::now() => Err(GitHubError::RateLimited {
                host: host.to_string(),
                retry_at: Some(until),
            }),
            _ => Ok(()),
        }
    }

    async fn send(
        &self,
        host: &str,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Answer, GitHubError> {
        let host = normalize_host(host);
        if !is_valid_host(&host) {
            return Err(GitHubError::Request {
                detail: format!("{host:?} is not a GitHub host name"),
                host,
            });
        }
        self.check_paused(&host)?;
        let token = self.credentials.token(&host).await?;
        let _slot = self.slots.acquire().await;
        // A pause can begin while this request waited for a slot.
        self.check_paused(&host)?;
        let request = Request::builder()
            .method(method)
            .uri(url)
            // A redirect, even to the same host over plain HTTP, would carry the token along.
            .follow_redirects(RedirectPolicy::NoFollow)
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
            self.paused_until.lock().insert(
                host.clone(),
                retry_at.unwrap_or(now + DEFAULT_RATE_LIMIT_PAUSE),
            );
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

    /// Runs a mutation: any GraphQL error, or no payload under `field`, is a failure, since a
    /// mutation that did not happen must not look done.
    async fn mutate(
        &self,
        host: &str,
        query: &str,
        variables: Value,
        field: &str,
    ) -> Result<(), GitHubError> {
        let data: Value = self.graphql_with(host, query, variables, true).await?;
        if data.get(field).is_none_or(Value::is_null) {
            return Err(GitHubError::Response {
                host: normalize_host(host),
                status: 200,
                messages: vec![format!("GitHub did not apply {field}.")],
            });
        }
        Ok(())
    }

    pub async fn graphql<T: DeserializeOwned>(
        &self,
        host: &str,
        query: &str,
        variables: Value,
    ) -> Result<T, GitHubError> {
        self.graphql_with(host, query, variables, false).await
    }

    async fn graphql_with<T: DeserializeOwned>(
        &self,
        host: &str,
        query: &str,
        variables: Value,
        errors_fail: bool,
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
        let errors_present = envelope
            .errors
            .as_ref()
            .is_some_and(|errors| !errors.is_empty());
        match (envelope.data, envelope.errors) {
            (Some(_), Some(errors)) if errors_fail && errors_present => {
                Err(GitHubError::Response {
                    host: normalize_host(host),
                    status: answer.status,
                    messages: errors
                        .into_iter()
                        .filter_map(|error| error.message)
                        .collect(),
                })
            }
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
                    messages: errors
                        .into_iter()
                        .filter_map(|error| error.message)
                        .collect(),
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
    /// GitHub filters by the exact head (`owner:branch`), so other forks' pull requests from a
    /// branch of the same name never crowd it out.
    pub async fn branch_pull_request(
        &self,
        branch: &BranchHead,
    ) -> Result<BranchPullRequests, GitHubError> {
        let target = &branch.target;
        let default_branch = self.default_branch(target).await?;
        let pulls: Vec<RestPull> = self
            .rest(
                &target.host,
                Method::GET,
                &format!(
                    "repos/{}/{}/pulls?state=all&sort=updated&direction=desc&per_page=100&head={}",
                    target.owner,
                    target.name,
                    query_value(&format!("{}:{}", branch.head.owner, branch.branch)),
                ),
                None,
            )
            .await?;
        let Some(pull) = choose_pull_request(&pulls, branch) else {
            return Ok(BranchPullRequests {
                default_branch: Some(default_branch),
                pull_request: None,
            });
        };
        let mut pull_request = PullRequest {
            number: pull.number,
            title: pull.title.clone(),
            url: pull.html_url.clone(),
            state: pull.state(),
            base_branch: pull.base.name.clone(),
            head_sha: Some(pull.head.sha.clone()),
            checks: Vec::new(),
            checks_complete: true,
            mergeable: Mergeable::Unknown,
            review: None,
        };
        if pull_request.state.is_open() {
            // An older Enterprise server may not know `isRequired`, and GraphQL rejects a
            // document naming an unknown field even behind `@include`.
            let query = pull_request_status_query(is_github_dot_com(&normalize_host(&target.host)));
            let mut contexts = Vec::new();
            let mut commit_oid: Option<String> = None;
            let mut after: Option<String> = None;
            let mut complete = false;
            for page in 0..STATUS_CONTEXT_PAGES {
                let status: StatusQueryData = self
                    .graphql(
                        &target.host,
                        &query,
                        json!({
                            "owner": target.owner,
                            "name": target.name,
                            "number": pull.number,
                            "after": after,
                        }),
                    )
                    .await?;
                let Some(status) = status
                    .repository
                    .and_then(|repository| repository.pull_request)
                else {
                    complete = page == 0;
                    break;
                };
                if page == 0 {
                    pull_request.mergeable = match status.mergeable.as_deref() {
                        Some("MERGEABLE") => Mergeable::Mergeable,
                        Some("CONFLICTING") => Mergeable::Conflicting,
                        _ => Mergeable::Unknown,
                    };
                    pull_request.review =
                        status
                            .review_decision
                            .as_deref()
                            .and_then(|decision| match decision {
                                "APPROVED" => Some(ReviewDecision::Approved),
                                "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
                                "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
                                _ => None,
                            });
                }
                let Some(commit) = status
                    .commits
                    .nodes
                    .into_iter()
                    .next()
                    .map(|node| node.commit)
                else {
                    complete = page == 0;
                    break;
                };
                // A push between pages would mix the checks of two commits.
                if page > 0 && commit_oid != commit.oid {
                    break;
                }
                commit_oid = commit.oid.clone();
                let Some(rollup) = commit.status_check_rollup else {
                    complete = page == 0;
                    break;
                };
                contexts.extend(rollup.contexts.nodes);
                match rollup.contexts.page_info {
                    Some(PageInfo {
                        has_next_page: true,
                        end_cursor: Some(cursor),
                    }) => after = Some(cursor),
                    _ => {
                        complete = true;
                        break;
                    }
                }
            }
            let mut checks = collapse_checks(contexts);
            let required_names = self.required_check_names(target, &pull.base.name).await;
            add_required_checks(&mut checks, &required_names);
            pull_request.checks = checks;
            pull_request.checks_complete = complete;
        }
        Ok(BranchPullRequests {
            default_branch: Some(default_branch),
            pull_request: Some(pull_request),
        })
    }

    /// The repository's default branch, which seldom changes, so it is read once an hour.
    async fn default_branch(&self, repository: &RepositoryLocator) -> Result<String, GitHubError> {
        let key = (
            normalize_host(&repository.host),
            repository.owner.to_lowercase(),
            repository.name.to_lowercase(),
        );
        if let Some((read_at, name)) = self.default_branches.lock().get(&key)
            && read_at.elapsed() < DEFAULT_BRANCH_TTL
        {
            return Ok(name.clone());
        }
        #[derive(Deserialize)]
        struct RestRepository {
            default_branch: String,
        }
        let read: RestRepository = self
            .rest(
                &repository.host,
                Method::GET,
                &format!("repos/{}/{}", repository.owner, repository.name),
                None,
            )
            .await?;
        self.default_branches
            .lock()
            .insert(key, (Instant::now(), read.default_branch.clone()));
        Ok(read.default_branch)
    }

    /// The names of the checks the base branch requires, from its rulesets and classic branch
    /// protection. GitHub waits for a required check that never reports (a workflow skipped by a
    /// path filter), which the pull request's own checks cannot show. A failed read leaves only
    /// the checks that report themselves as required.
    async fn required_check_names(
        &self,
        repository: &RepositoryLocator,
        branch: &str,
    ) -> Vec<String> {
        #[derive(Deserialize)]
        struct Rule {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            parameters: Option<RuleParameters>,
        }
        #[derive(Deserialize)]
        struct RuleParameters {
            #[serde(default)]
            required_status_checks: Vec<RequiredCheck>,
        }
        #[derive(Deserialize)]
        struct RequiredCheck {
            context: String,
        }
        #[derive(Deserialize)]
        struct Branch {
            #[serde(default)]
            protection: Option<Protection>,
        }
        #[derive(Deserialize)]
        struct Protection {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            required_status_checks: Option<ProtectionChecks>,
        }
        #[derive(Deserialize)]
        struct ProtectionChecks {
            #[serde(default)]
            enforcement_level: Option<String>,
            #[serde(default)]
            contexts: Vec<String>,
        }

        let key = (
            normalize_host(&repository.host),
            repository.owner.to_lowercase(),
            repository.name.to_lowercase(),
            branch.to_string(),
        );
        if let Some((read_at, names)) = self.required_checks.lock().get(&key)
            && read_at.elapsed() < REQUIRED_CHECKS_TTL
        {
            return names.clone();
        }
        let branch_path = query_value(branch);
        let mut names = Vec::new();
        match self
            .rest::<Vec<Rule>>(
                &repository.host,
                Method::GET,
                &format!(
                    "repos/{}/{}/rules/branches/{branch_path}",
                    repository.owner, repository.name
                ),
                None,
            )
            .await
        {
            Ok(rules) => names.extend(
                rules
                    .into_iter()
                    .filter(|rule| rule.kind == "required_status_checks")
                    .filter_map(|rule| rule.parameters)
                    .flat_map(|parameters| parameters.required_status_checks)
                    .map(|check| check.context),
            ),
            Err(error) => log::warn!("Could not read the rules of {branch}: {error}"),
        }
        match self
            .rest::<Branch>(
                &repository.host,
                Method::GET,
                &format!(
                    "repos/{}/{}/branches/{branch_path}",
                    repository.owner, repository.name
                ),
                None,
            )
            .await
        {
            Ok(Branch {
                protection:
                    Some(Protection {
                        enabled: true,
                        required_status_checks: Some(checks),
                    }),
            }) if checks.enforcement_level.as_deref() != Some("off") => {
                names.extend(checks.contexts)
            }
            Ok(_) => {}
            Err(error) => log::warn!("Could not read the protection of {branch}: {error}"),
        }
        names.sort();
        names.dedup();
        self.required_checks
            .lock()
            .insert(key, (Instant::now(), names.clone()));
        names
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
            Some({
                let mut body = json!({
                    "base": request.base,
                    "head": request.head.selector(),
                    "title": request.title,
                    "body": request.body,
                    "draft": request.draft,
                    // Match gh; the API default is undocumented.
                    "maintainer_can_modify": true,
                });
                // `owner:branch` cannot tell a fork from its parent when one organization owns
                // both, so GitHub requires the head repository then.
                if request.head.is_cross_repository() {
                    body["head_repo"] = Value::String(request.head.head.name.clone());
                }
                body
            }),
        )
        .await
    }

    /// The pull request's review threads with their first comments, page by page. Thread lines
    /// belong to the head commit, so a push between pages restarts the read.
    pub async fn review_threads(
        &self,
        repository: &RepositoryLocator,
        number: u64,
    ) -> Result<PullRequestReview, GitHubError> {
        for _ in 0..REVIEW_THREAD_ATTEMPTS {
            if let Some(review) = self.read_review_threads(repository, number).await? {
                return Ok(review);
            }
        }
        Err(GitHubError::Response {
            host: normalize_host(&repository.host),
            status: 0,
            messages: vec![
                "The pull request kept changing while its review was read; try again.".into(),
            ],
        })
    }

    /// One read of the review threads, or None when the head commit changed between pages.
    async fn read_review_threads(
        &self,
        repository: &RepositoryLocator,
        number: u64,
    ) -> Result<Option<PullRequestReview>, GitHubError> {
        let mut review: Option<PullRequestReview> = None;
        let mut after: Option<String> = None;
        for _ in 0..REVIEW_THREAD_PAGES {
            let data: ReviewQueryData = self
                .graphql(
                    &repository.host,
                    REVIEW_THREADS_QUERY,
                    json!({
                        "owner": repository.owner,
                        "name": repository.name,
                        "number": number,
                        "after": after,
                    }),
                )
                .await?;
            let pull_request = data
                .repository
                .and_then(|repository| repository.pull_request)
                .ok_or_else(|| GitHubError::NotFound {
                    host: normalize_host(&repository.host),
                })?;
            let page = pull_request.review_threads;
            if review
                .as_ref()
                .is_some_and(|review| review.head_sha != pull_request.head_ref_oid)
            {
                return Ok(None);
            }
            let review = review.get_or_insert_with(|| PullRequestReview {
                pull_request_id: pull_request.id.clone(),
                number,
                head_sha: pull_request.head_ref_oid.clone(),
                threads: Vec::new(),
            });
            review
                .threads
                .extend(page.nodes.into_iter().map(ReviewThreadNode::into_thread));
            match page.page_info {
                Some(PageInfo {
                    has_next_page: true,
                    end_cursor: Some(cursor),
                }) => after = Some(cursor),
                _ => break,
            }
        }
        review.map(Some).ok_or_else(|| GitHubError::NotFound {
            host: normalize_host(&repository.host),
        })
    }

    pub async fn reply_to_review_thread(
        &self,
        host: &str,
        thread_id: &str,
        body: &str,
    ) -> Result<(), GitHubError> {
        self.mutate(
            host,
            r#"mutation ReplyToReviewThread($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) { comment { id } }
}"#,
            json!({ "threadId": thread_id, "body": body }),
            "addPullRequestReviewThreadReply",
        )
        .await
    }

    pub async fn set_review_thread_resolved(
        &self,
        host: &str,
        thread_id: &str,
        resolved: bool,
    ) -> Result<(), GitHubError> {
        let (query, field) = if resolved {
            (
                "mutation ResolveReviewThread($threadId: ID!) { resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } } }",
                "resolveReviewThread",
            )
        } else {
            (
                "mutation UnresolveReviewThread($threadId: ID!) { unresolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } } }",
                "unresolveReviewThread",
            )
        };
        self.mutate(host, query, json!({ "threadId": thread_id }), field)
            .await
    }

    /// Comments on a line of the pull request's head commit. GitHub accepts lines inside the
    /// diff only.
    pub async fn add_review_comment(
        &self,
        repository: &RepositoryLocator,
        number: u64,
        comment: &NewReviewComment,
    ) -> Result<(), GitHubError> {
        self.rest::<Value>(
            &repository.host,
            Method::POST,
            &format!(
                "repos/{}/{}/pulls/{number}/comments",
                repository.owner, repository.name
            ),
            Some(json!({
                "body": comment.body,
                "commit_id": comment.commit_sha,
                "path": comment.path,
                "line": comment.line,
                "side": "RIGHT",
            })),
        )
        .await
        .map(drop)
    }

    /// Reviews `commit_sha`, the head the user looked at; without it GitHub would review whatever
    /// was pushed last.
    pub async fn submit_review(
        &self,
        repository: &RepositoryLocator,
        number: u64,
        commit_sha: &str,
        verdict: ReviewVerdict,
        body: &str,
    ) -> Result<(), GitHubError> {
        let event = match verdict {
            ReviewVerdict::Comment => "COMMENT",
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
        };
        let mut request = json!({ "event": event, "commit_id": commit_sha });
        if !body.is_empty() {
            request["body"] = Value::String(body.to_string());
        }
        self.rest::<Value>(
            &repository.host,
            Method::POST,
            &format!(
                "repos/{}/{}/pulls/{number}/reviews",
                repository.owner, repository.name
            ),
            Some(request),
        )
        .await
        .map(drop)
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
    /// False when some of the head commit's checks could not be read, so passing is unknown.
    pub checks_complete: bool,
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
        if self.checks_complete
            && gate
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

/// Bounds one status read: a hundred check contexts a page.
const STATUS_CONTEXT_PAGES: usize = 10;

fn pull_request_status_query(include_required: bool) -> String {
    let required = if include_required {
        "isRequired(pullRequestNumber: $number)"
    } else {
        ""
    };
    format!(
        r#"
query PullRequestStatus($owner: String!, $name: String!, $number: Int!, $after: String) {{
  repository(owner: $owner, name: $name) {{
    pullRequest(number: $number) {{
      mergeable
      reviewDecision
      commits(last: 1) {{
        nodes {{
          commit {{
            oid
            statusCheckRollup {{
              contexts(first: 100, after: $after) {{
                pageInfo {{ hasNextPage endCursor }}
                nodes {{
                  ... on StatusContext {{
                    context
                    state
                    targetUrl
                    createdAt
                    {required}
                  }}
                  ... on CheckRun {{
                    name
                    status
                    conclusion
                    detailsUrl
                    startedAt
                    completedAt
                    checkSuite {{ app {{ slug }} }}
                    {required}
                  }}
                }}
              }}
            }}
          }}
        }}
      }}
    }}
  }}
}}
"#
    )
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(Clone, Debug, Deserialize)]
struct Login {
    login: String,
}

/// A pull request as GitHub's REST list returns it.
#[derive(Clone, Debug, Deserialize)]
struct RestPull {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged_at: Option<String>,
    base: RestBase,
    head: RestHead,
}

#[derive(Clone, Debug, Deserialize)]
struct RestBase {
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RestHead {
    sha: String,
    /// `None` once the head repository is deleted.
    repo: Option<RestHeadRepository>,
}

#[derive(Clone, Debug, Deserialize)]
struct RestHeadRepository {
    full_name: String,
}

impl RestPull {
    fn state(&self) -> PullRequestState {
        if self.merged_at.is_some() {
            PullRequestState::Merged
        } else if self.state == "closed" {
            PullRequestState::Closed
        } else if self.draft {
            PullRequestState::Draft
        } else {
            PullRequestState::Open
        }
    }

    fn is_from(&self, branch: &BranchHead) -> bool {
        let head = format!("{}/{}", branch.head.owner, branch.head.name);
        self.head
            .repo
            .as_ref()
            .is_some_and(|repository| repository.full_name.eq_ignore_ascii_case(&head))
    }
}

/// GitHub lists the newest first; an open pull request wins over closed and merged ones. One
/// organization can own both a fork and its parent, so the head repository must match too.
fn choose_pull_request<'a>(pulls: &'a [RestPull], branch: &BranchHead) -> Option<&'a RestPull> {
    let mut matching = pulls.iter().filter(|pull| pull.is_from(branch));
    let first = matching.clone().next();
    matching.find(|pull| pull.state().is_open()).or(first)
}

/// Percent-encodes a query parameter value.
fn query_value(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
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
    #[serde(default)]
    oid: Option<String>,
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    contexts: CheckContexts,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckContexts {
    #[serde(default)]
    page_info: Option<PageInfo>,
    #[serde(default = "Vec::new")]
    nodes: Vec<CheckNode>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CheckSuite {
    app: Option<CheckApp>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CheckApp {
    slug: Option<String>,
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
    check_suite: Option<CheckSuite>,
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

/// One entry per check: a run still going outranks finished runs of the same check (a push and a
/// pull request workflow can both report it), else the latest finished run wins. A check is its
/// name together with the app reporting it, or a commit status: two apps' checks of one name are
/// different checks.
fn collapse_checks(nodes: Vec<CheckNode>) -> Vec<Check> {
    let mut latest: Vec<(Check, CheckIdentity, Option<String>)> = Vec::new();
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
        let identity = CheckIdentity {
            is_status: node.context.is_some() && node.name.is_none(),
            app: node
                .check_suite
                .as_ref()
                .and_then(|suite| suite.app.as_ref())
                .and_then(|app| app.slug.clone()),
        };
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
        match latest.iter_mut().find(|(existing, existing_identity, _)| {
            existing.name == check.name && *existing_identity == identity
        }) {
            Some((existing, _, existing_at)) => {
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
            None => latest.push((check, identity, at)),
        }
    }
    latest.into_iter().map(|(check, _, _)| check).collect()
}

#[derive(PartialEq)]
struct CheckIdentity {
    is_status: bool,
    app: Option<String>,
}

/// Marks the checks the base branch requires, and adds the required ones that have not reported
/// yet as pending, as GitHub waits for them.
fn add_required_checks(checks: &mut Vec<Check>, required_names: &[String]) {
    for name in required_names {
        let mut found = false;
        for check in checks.iter_mut().filter(|check| &check.name == name) {
            check.required = true;
            found = true;
        }
        if !found {
            checks.push(Check {
                name: name.clone(),
                status: CheckStatus::Pending,
                required: true,
                url: None,
            });
        }
    }
}

/// Bounds one read: a hundred threads a page, comments limited per thread.
const REVIEW_THREAD_PAGES: usize = 5;
const REVIEW_THREAD_ATTEMPTS: usize = 2;

/// Only ten comments ride with each thread: a hundred threads times a hundred comments made
/// GitHub charge a hundred points for one read.
const REVIEW_THREADS_QUERY: &str = r#"
query ReviewThreads($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id
      headRefOid
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          diffSide
          viewerCanReply
          viewerCanResolve
          viewerCanUnresolve
          comments(first: 10) {
            totalCount
            nodes { id author { login } body createdAt url }
          }
        }
      }
    }
  }
}
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    /// The line on the head commit; `None` once the line left the diff.
    pub line: Option<u32>,
    /// Whether `line` counts lines of the head commit. Comments on deleted lines count the
    /// base commit's lines instead, which the head's file does not have.
    pub on_head: bool,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub can_reply: bool,
    pub can_resolve: bool,
    pub can_unresolve: bool,
    pub comments: Vec<ReviewComment>,
    /// All comments, including those past the ones read.
    pub comment_count: usize,
}

impl ReviewThread {
    pub fn url(&self) -> Option<&str> {
        self.comments.first().map(|comment| comment.url.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestReview {
    pub pull_request_id: String,
    pub number: u64,
    pub head_sha: Option<String>,
    pub threads: Vec<ReviewThread>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewReviewComment {
    pub commit_sha: String,
    pub path: String,
    pub line: u32,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewVerdict {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Deserialize)]
struct ReviewQueryData {
    repository: Option<ReviewQueryRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewQueryRepository {
    pull_request: Option<ReviewQueryPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewQueryPullRequest {
    id: String,
    head_ref_oid: Option<String>,
    review_threads: ReviewThreadPage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewThreadPage {
    #[serde(default)]
    page_info: Option<PageInfo>,
    #[serde(default)]
    nodes: Vec<ReviewThreadNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewThreadNode {
    id: String,
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    path: String,
    line: Option<u32>,
    #[serde(default)]
    diff_side: Option<String>,
    #[serde(default)]
    viewer_can_reply: bool,
    #[serde(default)]
    viewer_can_resolve: bool,
    #[serde(default)]
    viewer_can_unresolve: bool,
    comments: ReviewCommentPage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewCommentPage {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    nodes: Vec<ReviewCommentNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewCommentNode {
    id: String,
    author: Option<Login>,
    body: String,
    created_at: String,
    url: String,
}

impl ReviewThreadNode {
    fn into_thread(self) -> ReviewThread {
        let comment_count = self.comments.total_count.max(self.comments.nodes.len());
        ReviewThread {
            id: self.id,
            path: self.path,
            line: self.line,
            on_head: self.diff_side.as_deref() != Some("LEFT"),
            is_resolved: self.is_resolved,
            is_outdated: self.is_outdated,
            can_reply: self.viewer_can_reply,
            can_resolve: self.viewer_can_resolve,
            can_unresolve: self.viewer_can_unresolve,
            comments: self
                .comments
                .nodes
                .into_iter()
                .map(|comment| ReviewComment {
                    id: comment.id,
                    // A deleted account has no author.
                    author: comment
                        .author
                        .map(|author| author.login)
                        .unwrap_or_else(|| "ghost".into()),
                    body: comment.body,
                    created_at: comment.created_at,
                    url: comment.url,
                })
                .collect(),
            comment_count,
        }
    }
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

    /// A pull request as GitHub's REST list returns it, newest first.
    fn pull(number: u64, state: &str, merged: bool, head_repository: &str) -> Value {
        json!({
            "number": number,
            "title": format!("PR {number}"),
            "html_url": format!("https://github.com/evb0110/evb-ide/pull/{number}"),
            "state": state,
            "draft": false,
            "merged_at": if merged { json!("2026-10-30T10:00:00Z") } else { Value::Null },
            "base": { "ref": "main" },
            "head": { "sha": "abc", "repo": { "full_name": head_repository } },
        })
    }

    fn pulls(values: Vec<Value>) -> Vec<RestPull> {
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
        assert_eq!(
            environment_token("github.com", &blank),
            Some("github".into())
        );
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
        assert_eq!(
            environment_token("tenant.ghe.com", &both),
            Some("gh".into())
        );
    }

    #[test]
    fn rejects_hosts_that_would_move_the_api_url() {
        for host in [
            "github.com",
            "GitHub.com",
            "acme.ghe.com",
            "git.example.com:8443",
        ] {
            assert!(is_valid_host(host), "{host}");
        }
        for host in [
            "",
            "attacker.example?.ghe.com",
            "attacker.example/.ghe.com",
            "user@github.com",
            "github.com:",
            "github.com:99999999",
            "-github.com",
            "github..com",
        ] {
            assert!(!is_valid_host(host), "{host}");
        }
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", u64::MAX.to_string().parse().unwrap());
        assert_eq!(retry_at(&headers, SystemTime::now()), None);
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset", u64::MAX.to_string().parse().unwrap());
        assert_eq!(retry_at(&headers, SystemTime::now()), None);
    }

    #[test]
    fn api_urls_per_host() {
        assert_eq!(
            api_urls("github.com").graphql,
            "https://api.github.com/graphql"
        );
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
        assert!(!is_rate_limited(
            403,
            &empty,
            None,
            "Resource not accessible"
        ));
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
        let list = pulls(vec![
            pull(3, "closed", false, "evb0110/evb-ide"),
            pull(2, "open", false, "evb0110/evb-ide"),
        ]);
        assert_eq!(
            choose_pull_request(&list, &branch).map(|pull| pull.number),
            Some(2)
        );

        let finished = pulls(vec![
            pull(6, "closed", true, "evb0110/evb-ide"),
            pull(5, "closed", false, "evb0110/evb-ide"),
        ]);
        let chosen = choose_pull_request(&finished, &branch).unwrap();
        assert_eq!(
            (chosen.number, chosen.state()),
            (6, PullRequestState::Merged)
        );

        // One organization can own a fork and its parent: `owner:branch` matches both, so the
        // head repository decides.
        let fork_branch = BranchHead {
            head: locator("evb0110", "evb-ide-fork"),
            ..branch
        };
        assert!(fork_branch.is_cross_repository());
        let same_owner = pulls(vec![
            pull(8, "open", false, "evb0110/evb-ide"),
            pull(9, "open", false, "evb0110/evb-ide-fork"),
        ]);
        assert_eq!(
            choose_pull_request(&same_owner, &fork_branch).map(|pull| pull.number),
            Some(9)
        );
        assert_eq!(
            query_value("contributor:feature/a b"),
            "contributor:feature%2Fa%20b"
        );
    }

    fn check(value: Value) -> CheckNode {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn collapses_and_summarizes_checks() {
        let checks = collapse_checks(vec![
            check(
                json!({ "name": "test", "status": "COMPLETED", "conclusion": "FAILURE", "completedAt": "2026-10-31T09:00:00Z" }),
            ),
            check(
                json!({ "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS", "completedAt": "2026-10-31T10:00:00Z" }),
            ),
            check(
                json!({ "name": "lint", "status": "IN_PROGRESS", "conclusion": null, "isRequired": true }),
            ),
            check(
                json!({ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS", "completedAt": "2026-10-31T11:00:00Z" }),
            ),
            check(
                json!({ "context": "ci/legacy", "state": "SUCCESS", "targetUrl": "https://ci.example.com/1" }),
            ),
            check(json!({ "name": "  " })),
            // Another app's check of the same name is a different check.
            check(
                json!({ "name": "test", "status": "COMPLETED", "conclusion": "FAILURE", "completedAt": "2026-10-31T12:00:00Z", "checkSuite": { "app": { "slug": "other-ci" } } }),
            ),
            check(json!({ "context": "test", "state": "PENDING" })),
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
                Check {
                    name: "test".into(),
                    status: CheckStatus::Failure,
                    required: false,
                    url: None,
                },
                Check {
                    name: "test".into(),
                    status: CheckStatus::Pending,
                    required: false,
                    url: None,
                },
            ]
        );
        assert!(pull_request_status_query(true).contains("isRequired"));
        assert!(!pull_request_status_query(false).contains("isRequired"));

        let pull_request = |checks: Vec<Check>| PullRequest {
            number: 1,
            title: String::new(),
            url: String::new(),
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_sha: None,
            checks,
            checks_complete: true,
            mergeable: Mergeable::Unknown,
            review: None,
        };
        let named = |name: &str, status: CheckStatus, required: bool| Check {
            name: name.into(),
            status,
            required,
            url: None,
        };
        // The other app's failing `test` is not hidden by the first app's success.
        assert_eq!(pull_request(checks).checks_summary(), ChecksSummary::Failed);
        assert_eq!(
            pull_request(Vec::new()).checks_summary(),
            ChecksSummary::None
        );
        // Checks that could not all be read cannot pass, but a failure among them still fails.
        let partial = |checks: Vec<Check>| PullRequest {
            checks_complete: false,
            ..pull_request(checks)
        };
        assert_eq!(
            partial(vec![named("a", CheckStatus::Success, false)]).checks_summary(),
            ChecksSummary::Pending
        );
        assert_eq!(
            partial(vec![named("a", CheckStatus::Failure, false)]).checks_summary(),
            ChecksSummary::Failed
        );
        // A required check that never reported keeps the pull request pending, and a reported
        // one becomes required even when GitHub did not say so.
        let mut checks = vec![named("build", CheckStatus::Success, false)];
        add_required_checks(&mut checks, &["build".to_string(), "security".to_string()]);
        assert_eq!(
            checks,
            vec![
                named("build", CheckStatus::Success, true),
                named("security", CheckStatus::Pending, true),
            ]
        );
        assert_eq!(
            pull_request(checks).checks_summary(),
            ChecksSummary::Pending
        );
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
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let http = FakeHttpClient::create({
            let requests = requests.clone();
            move |mut request| {
                let requests = requests.clone();
                async move {
                    assert_eq!(
                        request.headers().get("Authorization").unwrap(),
                        "Bearer secret"
                    );
                    let uri = request.uri().to_string();
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    if uri.ends_with("/graphql") {
                        let body: Value = serde_json::from_str(&body)?;
                        requests.lock().push(body["variables"].to_string());
                        return Ok(json_response(
                            200,
                            json!({ "data": { "repository": { "pullRequest": {
                                "mergeable": "CONFLICTING",
                                "reviewDecision": "APPROVED",
                                "commits": { "nodes": [{ "commit": { "statusCheckRollup": {
                                    "contexts": { "nodes": [
                                        { "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://ci/1" },
                                        {}
                                    ] }
                                } } }] }
                            } } } }),
                        ));
                    }
                    requests.lock().push(uri.clone());
                    Ok(if uri.contains("/rules/branches/") {
                        json_response(
                            200,
                            json!([
                                { "type": "deletion" },
                                { "type": "required_status_checks", "parameters": { "required_status_checks": [{ "context": "build" }] } }
                            ]),
                        )
                    } else if uri.contains("/branches/") {
                        json_response(
                            200,
                            json!({ "name": "main", "protection": { "enabled": false, "required_status_checks": { "enforcement_level": "off", "contexts": ["never"] } } }),
                        )
                    } else if uri.contains("/pulls?") {
                        json_response(
                            200,
                            json!([
                                pull(14, "open", false, "someone/evb-ide"),
                                pull(12, "open", false, "evb0110/evb-ide")
                            ]),
                        )
                    } else {
                        json_response(200, json!({ "default_branch": "main" }))
                    })
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let branch = same_repository_branch("feature/one");
        let result = client.branch_pull_request(&branch).await.unwrap();
        assert_eq!(result.default_branch.as_deref(), Some("main"));
        let pull_request = result.pull_request.unwrap();
        assert_eq!(pull_request.number, 12);
        assert_eq!(pull_request.state, PullRequestState::Open);
        assert_eq!(pull_request.mergeable, Mergeable::Conflicting);
        assert_eq!(pull_request.review, Some(ReviewDecision::Approved));
        assert_eq!(pull_request.checks_summary(), ChecksSummary::Passed);
        // The base branch's rules made `build` required; disabled protection added nothing.
        assert_eq!(
            pull_request
                .checks
                .iter()
                .map(|check| (check.name.as_str(), check.required))
                .collect::<Vec<_>>(),
            vec![("build", true)]
        );
        // The default branch and the required checks are read once and kept.
        client.branch_pull_request(&branch).await.unwrap();
        assert_eq!(
            *requests.lock(),
            vec![
                "https://api.github.com/repos/evb0110/evb-ide".to_string(),
                "https://api.github.com/repos/evb0110/evb-ide/pulls?state=all&sort=updated&direction=desc&per_page=100&head=evb0110:feature%2Fone".to_string(),
                json!({ "owner": "evb0110", "name": "evb-ide", "number": 12, "after": null }).to_string(),
                "https://api.github.com/repos/evb0110/evb-ide/rules/branches/main".to_string(),
                "https://api.github.com/repos/evb0110/evb-ide/branches/main".to_string(),
                "https://api.github.com/repos/evb0110/evb-ide/pulls?state=all&sort=updated&direction=desc&per_page=100&head=evb0110:feature%2Fone".to_string(),
                json!({ "owner": "evb0110", "name": "evb-ide", "number": 12, "after": null }).to_string(),
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
    async fn reads_review_threads_across_pages(cx: &mut gpui::TestAppContext) {
        let afters = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = FakeHttpClient::create({
            let afters = afters.clone();
            move |mut request| {
                let afters = afters.clone();
                async move {
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    let body: Value = serde_json::from_str(&body)?;
                    let after = body["variables"]["after"].clone();
                    afters.lock().push(after.clone());
                    let thread = |id: &str, line: Value, resolved: bool| {
                        json!({
                            "id": id,
                            "isResolved": resolved,
                            "isOutdated": line.is_null(),
                            "path": "src/main.rs",
                            "line": line,
                            "diffSide": if resolved { "LEFT" } else { "RIGHT" },
                            "viewerCanReply": true,
                            "viewerCanResolve": !resolved,
                            "viewerCanUnresolve": resolved,
                            "comments": {
                                "totalCount": 12,
                                "nodes": [{ "id": format!("{id}-c"), "author": null, "body": "Why?", "createdAt": "2026-10-31T10:00:00Z", "url": format!("https://github.com/o/r/pull/7#discussion_{id}") }]
                            }
                        })
                    };
                    let (nodes, page_info) = if after.is_null() {
                        (
                            json!([thread("a", json!(3), false)]),
                            json!({ "hasNextPage": true, "endCursor": "cursor-1" }),
                        )
                    } else {
                        (
                            json!([thread("b", Value::Null, true)]),
                            json!({ "hasNextPage": false, "endCursor": null }),
                        )
                    };
                    Ok(json_response(
                        200,
                        json!({ "data": { "repository": { "pullRequest": {
                            "id": "PR_node",
                            "headRefOid": "head",
                            "reviewThreads": { "pageInfo": page_info, "nodes": nodes }
                        } } } }),
                    ))
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let review = client.review_threads(&locator("o", "r"), 7).await.unwrap();
        assert_eq!(*afters.lock(), vec![Value::Null, json!("cursor-1")]);
        assert_eq!(review.pull_request_id, "PR_node");
        assert_eq!(review.head_sha.as_deref(), Some("head"));
        assert_eq!(
            review
                .threads
                .iter()
                .map(|thread| (
                    thread.id.as_str(),
                    thread.line,
                    thread.on_head,
                    thread.is_resolved,
                    thread.comment_count
                ))
                .collect::<Vec<_>>(),
            vec![
                ("a", Some(3), true, false, 12),
                ("b", None, false, true, 12)
            ]
        );
        assert_eq!(review.threads[0].comments[0].author, "ghost");
        assert_eq!(
            review.threads[0].url(),
            Some("https://github.com/o/r/pull/7#discussion_a")
        );
    }

    #[gpui::test]
    async fn rereads_review_threads_after_a_push_between_pages(cx: &mut gpui::TestAppContext) {
        let calls = Arc::new(AtomicUsize::new(0));
        let http = FakeHttpClient::create({
            let calls = calls.clone();
            move |_| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    // The second page arrives after a push; the reread sees one page at the new head.
                    let (head, page_info) = match call {
                        0 => (
                            "old",
                            json!({ "hasNextPage": true, "endCursor": "cursor-1" }),
                        ),
                        _ => ("new", json!({ "hasNextPage": false, "endCursor": null })),
                    };
                    Ok(json_response(
                        200,
                        json!({ "data": { "repository": { "pullRequest": {
                            "id": "PR_node",
                            "headRefOid": head,
                            "reviewThreads": { "pageInfo": page_info, "nodes": [] }
                        } } } }),
                    ))
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let review = client.review_threads(&locator("o", "r"), 7).await.unwrap();
        assert_eq!(review.head_sha.as_deref(), Some("new"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[gpui::test]
    async fn sends_review_writes(cx: &mut gpui::TestAppContext) {
        let sent = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let http = FakeHttpClient::create({
            let sent = sent.clone();
            move |mut request| {
                let sent = sent.clone();
                async move {
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    let body: Value = serde_json::from_str(&body)?;
                    let query = body["query"].as_str().unwrap_or_default().to_string();
                    sent.lock().push((request.uri().to_string(), body));
                    Ok(json_response(
                        200,
                        if query.contains("UnresolveReviewThread") {
                            // GitHub refuses with HTTP 200, a null payload and an error.
                            json!({ "data": { "unresolveReviewThread": null }, "errors": [{ "type": "FORBIDDEN", "message": "Resource not accessible by integration" }] })
                        } else if query.contains("ResolveReviewThread") {
                            json!({ "data": { "resolveReviewThread": { "thread": { "isResolved": true } } } })
                        } else if query.contains("ReplyToReviewThread") {
                            json!({ "data": { "addPullRequestReviewThreadReply": { "comment": { "id": "C" } } } })
                        } else {
                            json!({ "id": 1 })
                        },
                    ))
                }
            }
        });
        let client = GitHubClient::new(http, Arc::new(FixedToken), cx.executor());
        let repository = locator("o", "r");
        client
            .reply_to_review_thread("github.com", "T1", "Done")
            .await
            .unwrap();
        client
            .set_review_thread_resolved("github.com", "T1", true)
            .await
            .unwrap();
        assert_eq!(
            client
                .set_review_thread_resolved("github.com", "T1", false)
                .await
                .unwrap_err()
                .to_string(),
            "GitHub returned an error: Resource not accessible by integration"
        );
        client
            .add_review_comment(
                &repository,
                7,
                &NewReviewComment {
                    commit_sha: "head".into(),
                    path: "src/main.rs".into(),
                    line: 4,
                    body: "Nit".into(),
                },
            )
            .await
            .unwrap();
        client
            .submit_review(&repository, 7, "head", ReviewVerdict::Approve, "")
            .await
            .unwrap();
        let sent = sent.lock().clone();
        assert_eq!(
            sent[0].1["variables"],
            json!({ "threadId": "T1", "body": "Done" })
        );
        assert!(
            sent[1].1["query"]
                .as_str()
                .unwrap()
                .contains("resolveReviewThread")
        );
        assert_eq!(
            sent[3],
            (
                "https://api.github.com/repos/o/r/pulls/7/comments".to_string(),
                json!({ "body": "Nit", "commit_id": "head", "path": "src/main.rs", "line": 4, "side": "RIGHT" })
            )
        );
        assert_eq!(
            sent[4],
            (
                "https://api.github.com/repos/o/r/pulls/7/reviews".to_string(),
                json!({ "event": "APPROVE", "commit_id": "head" })
            )
        );
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
                    "head_repo": "evb-ide",
                    "title": "Title",
                    "body": "Body",
                    "draft": true,
                    "maintainer_can_modify": true,
                })
            ))
        );
    }
}

use crate::config::Selection;
use crate::util::is_safe_component;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, HeaderMap};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// The subset of GitHub's repository object that we use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub owner: Owner,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub disabled: bool,
    pub ssh_url: String,
    pub clone_url: String,
    /// Size in KB as estimated by GitHub.
    #[serde(default)]
    pub size: u64,
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub has_wiki: bool,
    pub default_branch: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub login: String,
}

/// Where repository metadata comes from.
pub enum Backend {
    Api(ApiClient),
    Gh(GhClient),
}

impl Backend {
    async fn list(&self, path: &str) -> Result<Vec<Repo>> {
        let path = with_per_page(path);
        match self {
            Backend::Api(c) => c.paginate(&path).await,
            Backend::Gh(c) => c.paginate(&path).await,
        }
    }

    async fn get(&self, path: &str) -> Result<Repo> {
        match self {
            Backend::Api(c) => c.get_json(path).await,
            Backend::Gh(c) => c.get_json(path).await,
        }
    }

    pub async fn login(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let u: User = match self {
            Backend::Api(c) => c.get_json("user").await?,
            Backend::Gh(c) => c.get_json("user").await?,
        };
        Ok(u.login)
    }
}

fn with_per_page(path: &str) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}per_page=100")
}

pub struct Discovered {
    /// Every repository found (before filtering), sorted by full name.
    pub repos: Vec<Repo>,
    /// Sources that failed. When non-empty, the list may be incomplete.
    pub errors: Vec<String>,
}

/// Query every configured source concurrently and merge the results.
pub async fn discover(backend: &Backend, sel: &Selection, authenticated: bool) -> Discovered {
    let mut errors = Vec::new();
    let mut lists: Vec<(String, String)> = Vec::new(); // (label, api path)
    let mut singles: Vec<(String, String)> = Vec::new();

    let need_login = !sel.users.is_empty() && authenticated;
    let login = if need_login {
        match backend.login().await {
            Ok(l) => Some(l),
            Err(e) => {
                errors.push(format!("looking up the authenticated user: {e:#}"));
                None
            }
        }
    } else {
        None
    };

    if sel.mine() {
        if authenticated {
            lists.push((
                "your repositories".into(),
                format!("user/repos?affiliation={}", sel.affiliation.join(",")),
            ));
        } else {
            errors.push(
                "listing your repositories needs a GitHub token (set GITHUB_TOKEN, configure \
                 token_command, or `gh auth login`)"
                    .into(),
            );
        }
    }
    for user in &sel.users {
        if !is_safe_component(user) {
            errors.push(format!("invalid user name {user:?}"));
            continue;
        }
        let path = match &login {
            // /users/{me}/repos only returns public repositories.
            Some(me) if me.eq_ignore_ascii_case(user) => "user/repos?affiliation=owner".to_owned(),
            _ => format!("users/{user}/repos?type=owner"),
        };
        lists.push((format!("user {user}"), path));
    }
    for org in &sel.orgs {
        if !is_safe_component(org) {
            errors.push(format!("invalid organization name {org:?}"));
            continue;
        }
        lists.push((format!("org {org}"), format!("orgs/{org}/repos?type=all")));
    }
    if sel.starred {
        if authenticated {
            lists.push(("starred repositories".into(), "user/starred".into()));
        } else {
            errors.push("listing starred repositories needs a GitHub token".into());
        }
    }
    for full in &sel.repos {
        match full.split_once('/') {
            Some((o, n)) if is_safe_component(o) && is_safe_component(n) => {
                singles.push((full.clone(), format!("repos/{o}/{n}")));
            }
            _ => errors.push(format!("invalid repository {full:?}; expected OWNER/NAME")),
        }
    }

    let list_results = futures::future::join_all(lists.iter().map(|(label, path)| async move {
        (label, backend.list(path).await)
    }));
    let single_results = futures::future::join_all(singles.iter().map(|(label, path)| async move {
        (label, backend.get(path).await)
    }));
    let (list_results, single_results) = tokio::join!(list_results, single_results);

    let mut by_id: HashMap<u64, Repo> = HashMap::new();
    for (label, res) in list_results {
        match res {
            Ok(repos) => {
                for r in repos {
                    by_id.entry(r.id).or_insert(r);
                }
            }
            Err(e) => errors.push(format!("{label}: {e:#}")),
        }
    }
    for (label, res) in single_results {
        match res {
            Ok(r) => {
                by_id.entry(r.id).or_insert(r);
            }
            Err(e) => errors.push(format!("{label}: {e:#}")),
        }
    }

    let mut repos: Vec<Repo> = Vec::with_capacity(by_id.len());
    for r in by_id.into_values() {
        if is_safe_component(&r.owner.login) && is_safe_component(&r.name) {
            repos.push(r);
        } else {
            errors.push(format!("skipping repository with unexpected name {:?}", r.full_name));
        }
    }
    repos.sort_by_key(|r| r.full_name.to_ascii_lowercase());
    Discovered { repos, errors }
}

// ---------------------------------------------------------------------------
// REST API client
// ---------------------------------------------------------------------------

pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

pub struct ApiUser {
    pub login: String,
    pub scopes: Option<String>,
    pub rate_remaining: Option<String>,
}

impl ApiClient {
    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("code-backup/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .context("building HTTP client")?;
        Ok(ApiClient {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    async fn request(&self, url: &str) -> Result<reqwest::Response> {
        const ATTEMPTS: u32 = 4;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .get(url)
                .header(ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < ATTEMPTS && (e.is_timeout() || e.is_connect() || e.is_request()) => {
                    tracing::debug!("GET {url} failed ({e}); retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(anyhow!(e).context(format!("GET {}", strip_base(url, &self.base)))),
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            if (status.as_u16() == 403 || status.as_u16() == 429)
                && let Some(wait) = rate_limit_wait(resp.headers()) {
                    if wait <= Duration::from_secs(120) && attempt < ATTEMPTS {
                        tracing::warn!("GitHub rate limit hit; waiting {}s", wait.as_secs());
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    bail!(
                        "GitHub API rate limit exceeded; it resets in {}",
                        humantime::format_duration(Duration::from_secs(wait.as_secs()))
                    );
                }
            if status.is_server_error() && attempt < ATTEMPTS {
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
                .unwrap_or_else(|| body.chars().take(200).collect());
            let hint = if status.as_u16() == 401 {
                " (is the token valid?)"
            } else {
                ""
            };
            bail!("GET {}: {status}: {message}{hint}", strip_base(url, &self.base));
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        let resp = self.request(&url).await?;
        resp.json().await.with_context(|| format!("decoding {path}"))
    }

    pub async fn paginate<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(self.url(path));
        while let Some(url) = next.take() {
            let resp = self.request(&url).await?;
            next = next_link(resp.headers());
            let page: Vec<T> = resp.json().await.with_context(|| format!("decoding {path}"))?;
            out.extend(page);
        }
        Ok(out)
    }

    /// The authenticated user plus token scopes and remaining rate limit.
    pub async fn user_info(&self) -> Result<ApiUser> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let resp = self.request(&self.url("user")).await?;
        let h = resp.headers();
        let header = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let scopes = header("x-oauth-scopes");
        let rate_remaining = header("x-ratelimit-remaining");
        let u: User = resp.json().await?;
        Ok(ApiUser {
            login: u.login,
            scopes,
            rate_remaining,
        })
    }
}

fn strip_base<'a>(url: &'a str, base: &str) -> &'a str {
    url.strip_prefix(base).unwrap_or(url)
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(6)))
}

fn rate_limit_wait(h: &HeaderMap) -> Option<Duration> {
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
    if let Some(secs) = get("retry-after").and_then(|v| v.parse::<u64>().ok()) {
        return Some(Duration::from_secs(secs.max(1)));
    }
    if get("x-ratelimit-remaining") == Some("0") {
        let reset = get("x-ratelimit-reset")?.parse::<i64>().ok()?;
        let now = chrono::Utc::now().timestamp();
        return Some(Duration::from_secs((reset - now).max(1) as u64));
    }
    None
}

/// Parse `Link: <url>; rel="next", <url>; rel="last"`.
fn next_link(h: &HeaderMap) -> Option<String> {
    let link = h.get("link")?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let mut pieces = part.split(';');
        let url = pieces.next()?.trim();
        let is_next = pieces.any(|p| p.trim() == r#"rel="next""#);
        is_next.then(|| url.trim_start_matches('<').trim_end_matches('>').to_owned())
    })
}

// ---------------------------------------------------------------------------
// gh CLI backend
// ---------------------------------------------------------------------------

pub struct GhClient {
    gh: PathBuf,
    hostname: Option<String>,
}

impl GhClient {
    pub fn new(host: &str) -> Result<Self> {
        let gh = crate::util::which("gh")
            .context("--discovery gh needs the gh CLI on PATH (https://cli.github.com)")?;
        Ok(GhClient {
            gh,
            hostname: (host != "github.com").then(|| host.to_owned()),
        })
    }

    async fn api(&self, extra: &[&str]) -> Result<String> {
        let mut cmd = tokio::process::Command::new(&self.gh);
        cmd.arg("api");
        if let Some(h) = &self.hostname {
            cmd.args(["--hostname", h]);
        }
        cmd.args(extra)
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let out = cmd.output().await.context("running gh")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            bail!("gh api {}: {}", extra.last().unwrap_or(&""), err);
        }
        String::from_utf8(out.stdout).context("gh printed invalid UTF-8")
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let text = self.api(&[path]).await?;
        serde_json::from_str(&text).with_context(|| format!("decoding gh api {path}"))
    }

    /// `gh api --paginate --jq '.[]'` prints one JSON object per line.
    async fn paginate<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let text = self.api(&["--paginate", "--jq", ".[]", path]).await?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).with_context(|| format!("decoding gh api {path}")))
            .collect()
    }
}

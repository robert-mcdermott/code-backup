//! Per-repository git operations: create or update one mirror or checkout.

use crate::config::Mode;
use crate::discover::Repo;
use crate::git::{Git, GitError};
use crate::state::Kind;
use crate::util::{partial_path, plural};
use indicatif::ProgressBar;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct Ctx {
    pub git: Git,
    pub root: PathBuf,
    pub mode: Mode,
    pub pull_refs: bool,
    pub lfs: bool,
    pub retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Clone,
    Update,
}

pub struct Job {
    pub repo: Arc<Repo>,
    pub kind: Kind,
    /// Path relative to the backup directory, e.g. `owner/name.git`.
    pub rel: String,
    pub url: String,
    pub fingerprint: String,
    pub action: Action,
    /// Settings changed since the last sync; redo post-fetch steps (HEAD, LFS).
    pub refresh: bool,
}

impl Job {
    pub fn label(&self) -> String {
        match self.kind {
            Kind::Repo => self.repo.full_name.clone(),
            Kind::Wiki => format!("{} (wiki)", self.repo.full_name),
        }
    }

    fn preferred_head(&self) -> Option<&str> {
        match self.kind {
            Kind::Repo => self.repo.default_branch.as_deref(),
            Kind::Wiki => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Cloned,
    Updated,
    Unchanged,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub status: Status,
    pub detail: Option<String>,
    pub warnings: Vec<String>,
    /// The remote does not exist (e.g. a wiki that was never created).
    pub absent: bool,
}

impl Outcome {
    pub fn new(status: Status) -> Self {
        Outcome {
            status,
            detail: None,
            warnings: Vec::new(),
            absent: false,
        }
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
}

pub async fn execute(ctx: &Ctx, job: &Job, pb: &ProgressBar) -> Outcome {
    let path = ctx.root.join(&job.rel);
    let res = match (ctx.mode, job.action) {
        (Mode::Mirror, Action::Clone) => mirror_clone(ctx, job, &path, pb).await,
        (Mode::Mirror, Action::Update) => mirror_update(ctx, job, &path, pb).await,
        (Mode::Checkout, Action::Clone) => checkout_clone(ctx, job, &path, pb).await,
        (Mode::Checkout, Action::Update) => checkout_update(ctx, job, &path, pb).await,
    };
    match res {
        Ok(o) => o,
        Err(GitError::Cancelled) => Outcome::new(Status::Cancelled),
        Err(e) if job.kind == Kind::Wiki && e.is_not_found() => match job.action {
            Action::Clone => {
                let mut o = Outcome::new(Status::Skipped).detail("no wiki");
                o.absent = true;
                o
            }
            Action::Update => Outcome::new(Status::Skipped).detail("wiki no longer available on GitHub"),
        },
        Err(e) => Outcome::new(Status::Failed).detail(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Mirror mode
// ---------------------------------------------------------------------------

fn mirror_refspecs(pull_refs: bool) -> Vec<&'static str> {
    if pull_refs {
        vec!["+refs/*:refs/*"]
    } else {
        vec![
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
            "+refs/notes/*:refs/notes/*",
        ]
    }
}

async fn mirror_clone(ctx: &Ctx, job: &Job, path: &Path, pb: &ProgressBar) -> Result<Outcome, GitError> {
    let (parent, tmp, tmp_name) = prepare_tmp(path)?;
    // init + fetch (instead of `clone --mirror`) lets us choose the refspecs,
    // so refs/pull/* is not downloaded unless asked for.
    ctx.git
        .run(Some(&parent), &["init", "--bare", "--quiet", &tmp_name])
        .await?;
    let result: Result<(), GitError> = async {
        set_remote(ctx, &tmp, &job.url, Some(&mirror_refspecs(ctx.pull_refs))).await?;
        pb.set_message("cloning");
        fetch(ctx, &tmp).await?;
        set_head(ctx, &tmp, job.preferred_head()).await?;
        if ctx.lfs {
            lfs_fetch(ctx, &tmp, pb).await?;
        }
        Ok(())
    }
    .await;
    finish_clone(result, &tmp, path)
}

async fn mirror_update(ctx: &Ctx, job: &Job, path: &Path, pb: &ProgressBar) -> Result<Outcome, GitError> {
    let is_bare = path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir();
    if !is_bare {
        return Err(GitError::Io(format!(
            "{} exists but is not a bare git repository; move or delete it",
            path.display()
        )));
    }
    set_remote(ctx, path, &job.url, Some(&mirror_refspecs(ctx.pull_refs))).await?;
    pb.set_message("fetching");
    let changed = fetch(ctx, path).await?;
    if changed > 0 || job.refresh {
        set_head(ctx, path, job.preferred_head()).await?;
        if ctx.lfs {
            lfs_fetch(ctx, path, pb).await?;
        }
    }
    Ok(if changed > 0 {
        Outcome::new(Status::Updated).detail(refs_detail(changed))
    } else {
        Outcome::new(Status::Unchanged)
    })
}

/// Point HEAD at the default branch (or main/master/the first branch).
async fn set_head(ctx: &Ctx, dir: &Path, preferred: Option<&str>) -> Result<(), GitError> {
    let out = ctx
        .git
        .run(Some(dir), &["for-each-ref", "--format=%(refname)", "refs/heads/"])
        .await?;
    let branches: Vec<&str> = out.stdout.lines().filter(|l| !l.is_empty()).collect();
    let Some(first) = branches.first() else {
        return Ok(()); // empty repository
    };
    let pick = preferred
        .into_iter()
        .chain(["main", "master"])
        .map(|b| format!("refs/heads/{b}"))
        .find(|r| branches.contains(&r.as_str()))
        .unwrap_or_else(|| (*first).to_owned());
    let current = std::fs::read_to_string(dir.join("HEAD")).unwrap_or_default();
    if current.trim() != format!("ref: {pick}") {
        ctx.git.run(Some(dir), &["symbolic-ref", "HEAD", &pick]).await?;
    }
    Ok(())
}

/// Fetch LFS objects for every ref, but only for repositories that use LFS
/// (scanning the whole history of non-LFS repositories is wasted work).
async fn lfs_fetch(ctx: &Ctx, dir: &Path, pb: &ProgressBar) -> Result<(), GitError> {
    let attrs = ctx.git.output(Some(dir), &["show", "HEAD:.gitattributes"]).await?;
    if attrs.code != Some(0) || !attrs.stdout.contains("filter=lfs") {
        return Ok(());
    }
    pb.set_message("fetching LFS objects");
    retry(ctx, async || {
        ctx.git
            .run(Some(dir), &["lfs", "fetch", "--all", "origin"])
            .await
            .map(|_| ())
    })
    .await
}

// ---------------------------------------------------------------------------
// Checkout mode
// ---------------------------------------------------------------------------

async fn checkout_clone(ctx: &Ctx, job: &Job, path: &Path, pb: &ProgressBar) -> Result<Outcome, GitError> {
    let (parent, tmp, tmp_name) = prepare_tmp(path)?;
    pb.set_message("cloning");
    let result = retry(ctx, async || {
        if tmp.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        ctx.git
            .run(Some(&parent), &["clone", "--quiet", "--", &job.url, &tmp_name])
            .await
            .map(|_| ())
    })
    .await;
    finish_clone(result, &tmp, path)
}

async fn checkout_update(ctx: &Ctx, job: &Job, path: &Path, pb: &ProgressBar) -> Result<Outcome, GitError> {
    if !path.join(".git").exists() {
        return Err(GitError::Io(format!(
            "{} exists but is not a git working tree; move or delete it",
            path.display()
        )));
    }
    set_remote(ctx, path, &job.url, None).await?;
    pb.set_message("fetching");
    let changed = fetch(ctx, path).await?;

    let out = ctx
        .git
        .run(
            Some(path),
            &["status", "--porcelain=v2", "--branch", "--untracked-files=no"],
        )
        .await?;
    let st = parse_status(&out.stdout);

    let mut outcome = if changed > 0 {
        Outcome::new(Status::Updated).detail(refs_detail(changed))
    } else {
        Outcome::new(Status::Unchanged)
    };
    if st.behind > 0 {
        let branch = st.branch.as_deref().unwrap_or("HEAD");
        let blocked = if st.dirty {
            Some("uncommitted local changes".to_owned())
        } else if st.ahead > 0 {
            Some(format!("{} not on GitHub", plural(st.ahead as usize, "local commit", "local commits")))
        } else {
            None
        };
        match blocked {
            None => {
                pb.set_message("fast-forwarding");
                ctx.git
                    .run(Some(path), &["merge", "--ff-only", "--quiet", "@{upstream}"])
                    .await?;
                outcome = Outcome::new(Status::Updated)
                    .detail(format!("{branch} fast-forwarded {}", plural(st.behind as usize, "commit", "commits")));
            }
            Some(reason) => outcome.warnings.push(format!(
                "{branch} is {} behind GitHub but was not fast-forwarded: {reason}",
                plural(st.behind as usize, "commit", "commits")
            )),
        }
    }
    Ok(outcome)
}

#[derive(Default)]
struct WorktreeStatus {
    branch: Option<String>,
    ahead: u64,
    behind: u64,
    dirty: bool,
}

fn parse_status(s: &str) -> WorktreeStatus {
    let mut st = WorktreeStatus::default();
    for line in s.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            st.branch = (head != "(detached)").then(|| head.to_owned());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    st.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    st.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            st.dirty = true;
        }
    }
    st
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn refs_detail(n: usize) -> String {
    if n == 1 { "1 ref".into() } else { format!("{n} refs") }
}

/// Make sure the parent exists and no stale `.partial` directory is left over.
fn prepare_tmp(path: &Path) -> Result<(PathBuf, PathBuf, String), GitError> {
    let parent = path.parent().expect("repository path has a parent").to_path_buf();
    std::fs::create_dir_all(&parent)
        .map_err(|e| GitError::io(&format!("creating {}", parent.display()), e))?;
    let tmp = partial_path(path);
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .map_err(|e| GitError::io(&format!("removing {}", tmp.display()), e))?;
    }
    let name = tmp
        .file_name()
        .expect("partial path has a file name")
        .to_string_lossy()
        .into_owned();
    Ok((parent, tmp, name))
}

/// New repositories are built in `<path>.partial` and renamed into place only
/// when complete, so an interrupted clone never looks like a valid backup.
fn finish_clone(result: Result<(), GitError>, tmp: &Path, path: &Path) -> Result<Outcome, GitError> {
    match result {
        Ok(()) => {
            std::fs::rename(tmp, path)
                .map_err(|e| GitError::io(&format!("moving clone into {}", path.display()), e))?;
            Ok(Outcome::new(Status::Cloned))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(tmp);
            Err(e)
        }
    }
}

/// Set `origin`'s URL (and refspecs) only when they differ, so switching
/// transport or the pull-refs option applies to existing backups.
async fn set_remote(ctx: &Ctx, dir: &Path, url: &str, refspecs: Option<&[&str]>) -> Result<(), GitError> {
    let out = ctx
        .git
        .output(Some(dir), &["config", "--get-regexp", r"^remote\.origin\.(url|fetch)$"])
        .await?;
    if !matches!(out.code, Some(0) | Some(1)) {
        return Err(GitError::Failed {
            cmd: "config".into(),
            code: out.code,
            stderr: out.stderr,
        });
    }
    let mut cur_url = None;
    let mut cur_specs: Vec<&str> = Vec::new();
    for line in out.stdout.lines() {
        match line.split_once(' ') {
            Some(("remote.origin.url", v)) => cur_url = Some(v),
            Some(("remote.origin.fetch", v)) => cur_specs.push(v),
            _ => {}
        }
    }
    if cur_url != Some(url) {
        ctx.git
            .run(Some(dir), &["config", "remote.origin.url", url])
            .await?;
    }
    if let Some(specs) = refspecs
        && cur_specs != specs {
            if !cur_specs.is_empty() {
                ctx.git
                    .run(Some(dir), &["config", "--unset-all", "remote.origin.fetch"])
                    .await?;
            }
            for spec in specs {
                ctx.git
                    .run(Some(dir), &["config", "--add", "remote.origin.fetch", spec])
                    .await?;
            }
        }
    Ok(())
}

/// `git fetch --prune`; returns the number of refs that changed.
async fn fetch(ctx: &Ctx, dir: &Path) -> Result<usize, GitError> {
    let out = retry(ctx, async || {
        ctx.git
            .run(
                Some(dir),
                &["fetch", "--prune", "--no-show-forced-updates", "origin"],
            )
            .await
    })
    .await?;
    // Each updated, new or deleted ref prints one "old -> new" line.
    Ok(out.stderr.lines().filter(|l| l.contains(" -> ")).count())
}

/// Retry transient failures with exponential backoff (2s, 4s, 8s, ...).
async fn retry<T>(ctx: &Ctx, mut op: impl AsyncFnMut() -> Result<T, GitError>) -> Result<T, GitError> {
    let mut attempt = 0;
    loop {
        match op().await {
            Err(e) if attempt < ctx.retries && e.is_retryable() => {
                attempt += 1;
                let delay = Duration::from_secs(2u64.pow(attempt.min(6)));
                tracing::debug!("retrying in {}s after: {e}", delay.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = ctx.git.cancel_token().cancelled() => return Err(GitError::Cancelled),
                }
            }
            other => return other,
        }
    }
}

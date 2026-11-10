use crate::auth::{self, Token};
use crate::config::{Discovery, Mode, Orphans, Settings, Transport};
use crate::discover::{ApiClient, Backend, Discovered, GhClient, Repo, discover};
use crate::filter::Filter;
use crate::git::Git;
use crate::ops::{self, Action, Ctx, Job, Outcome, Status};
use crate::state::{Entry, Kind, ORPHAN_DIR, RunSummary, State, Store, read_state_for};
use crate::ui::Ui;
use crate::util::{fmt_duration, git_host_from_api, plural};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use console::style;
use futures::StreamExt;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How we reach GitHub for listing repositories (and the token, if any).
pub struct Access {
    pub host: String,
    pub token: Option<Token>,
    pub backend: Backend,
    pub authenticated: bool,
}

pub async fn connect(settings: &Settings, for_git: bool) -> Result<Access> {
    let host = git_host_from_api(&settings.api_url);
    let need_token = settings.discovery == Discovery::Api
        || (for_git && settings.transport == Transport::Https);
    let token = if need_token {
        auth::resolve_token(settings, &host).await?
    } else {
        None
    };
    if let Some(t) = &token {
        tracing::debug!("using GitHub token from {}", t.source);
    }
    let (backend, authenticated) = match settings.discovery {
        Discovery::Api => (
            Backend::Api(ApiClient::new(
                &settings.api_url,
                token.as_ref().map(|t| t.value.clone()),
            )?),
            token.is_some(),
        ),
        Discovery::Gh => (Backend::Gh(GhClient::new(&host)?), true),
    };
    Ok(Access {
        host,
        token,
        backend,
        authenticated,
    })
}

/// Resolves on Ctrl-C or SIGTERM.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub fn rel_path(repo: &Repo, kind: Kind, mode: Mode) -> String {
    let suffix = match (kind, mode) {
        (Kind::Repo, Mode::Mirror) => ".git",
        (Kind::Repo, Mode::Checkout) => "",
        (Kind::Wiki, Mode::Mirror) => ".wiki.git",
        (Kind::Wiki, Mode::Checkout) => ".wiki",
    };
    format!("{}/{}{}", repo.owner.login, repo.name, suffix)
}

fn remote_url(repo: &Repo, kind: Kind, transport: Transport) -> String {
    let base = match transport {
        Transport::Ssh => &repo.ssh_url,
        Transport::Https | Transport::Gh => &repo.clone_url,
    };
    match kind {
        Kind::Repo => base.clone(),
        Kind::Wiki => format!("{}.wiki.git", base.strip_suffix(".git").unwrap_or(base)),
    }
}

fn fingerprint(s: &Settings) -> String {
    format!(
        "{:?};pull_refs={};lfs={}",
        s.mode, s.pull_refs as u8, s.lfs as u8
    )
    .to_ascii_lowercase()
}

enum Decision {
    Run(Action, bool),
    Skip(Outcome),
}

fn decide(kind: Kind, exists: bool, prev: Option<&Entry>, repo: &Repo, fp: &str, force: bool) -> Decision {
    let refresh = prev.is_none_or(|p| p.fingerprint != fp);
    match kind {
        Kind::Repo if !exists => Decision::Run(Action::Clone, true),
        Kind::Repo => {
            let unchanged = !force
                && prev.is_some_and(|p| {
                    p.last_error.is_none()
                        && p.fingerprint == fp
                        && p.pushed_at.is_some()
                        && p.pushed_at == repo.pushed_at
                });
            if unchanged {
                Decision::Skip(Outcome::new(Status::Unchanged).detail("no pushes since last sync"))
            } else {
                Decision::Run(Action::Update, refresh)
            }
        }
        // GitHub's pushed_at does not track wiki edits, so existing wikis are
        // always fetched. Missing wikis are re-checked when the repo changes.
        Kind::Wiki if !exists => {
            let known_absent = !force && prev.is_some_and(|p| p.absent && p.pushed_at == repo.pushed_at);
            if known_absent {
                let mut o = Outcome::new(Status::Skipped).detail("no wiki");
                o.absent = true;
                Decision::Skip(o)
            } else {
                Decision::Run(Action::Clone, true)
            }
        }
        Kind::Wiki => Decision::Run(Action::Update, refresh),
    }
}

/// A repository whose id we already back up under a different path was
/// renamed or transferred on GitHub.
fn find_renamed(state: &State, repo: &Repo, kind: Kind, mode: Mode, rel: &str, root: &Path) -> Option<String> {
    state
        .entries
        .iter()
        .find(|(k, e)| {
            e.id == repo.id && e.kind == kind && e.mode == mode && k.as_str() != rel && root.join(k).exists()
        })
        .map(|(k, _)| k.clone())
}

#[derive(Serialize)]
struct JobResult {
    name: String,
    kind: Kind,
    path: String,
    status: Status,
    detail: Option<String>,
    warnings: Vec<String>,
    seconds: f64,
}

#[derive(Serialize, Default)]
struct Counts {
    cloned: usize,
    updated: usize,
    unchanged: usize,
    skipped: usize,
    failed: usize,
    cancelled: usize,
}

impl Counts {
    fn from(results: &[JobResult]) -> Self {
        let mut c = Counts::default();
        for r in results {
            match r.status {
                Status::Cloned => c.cloned += 1,
                Status::Updated => c.updated += 1,
                Status::Unchanged => c.unchanged += 1,
                Status::Skipped => c.skipped += 1,
                Status::Failed => c.failed += 1,
                Status::Cancelled => c.cancelled += 1,
            }
        }
        c
    }
}

#[derive(Serialize)]
struct Report<'a> {
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration_secs: f64,
    dest: &'a Path,
    mode: Mode,
    transport: Transport,
    dry_run: bool,
    interrupted: bool,
    discovered: usize,
    selected: usize,
    excluded: usize,
    discovery_errors: &'a [String],
    counts: Counts,
    orphans: &'a [String],
    archive: Option<String>,
    repos: &'a [JobResult],
}

pub async fn run(settings: Settings, ui: Ui) -> Result<ExitCode> {
    let started_at = Utc::now();
    let t0 = Instant::now();
    let dest = settings.require_dest()?;
    let root = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()?.join(dest)
    };
    let dry = settings.dry_run;

    // Lock the backup directory for the whole run (dry runs only read).
    let (store, mut state) = if dry {
        (None, read_state_for(&root)?)
    } else {
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let store = Store::open(&root)?;
        let state = store.load()?;
        (Some(store), state)
    };
    state.version = 1;

    preflight(&settings)?;
    let access = connect(&settings, true).await?;

    ui.info(format!(
        "Discovering repositories via {}...",
        match settings.discovery {
            Discovery::Api => "the GitHub API",
            Discovery::Gh => "gh",
        }
    ));
    let disc: Discovered = discover(&access.backend, &settings.select, access.authenticated).await;
    for e in &disc.errors {
        ui.error(e);
    }
    if disc.repos.is_empty() && !disc.errors.is_empty() {
        bail!("could not list any repositories");
    }

    let filter = Filter::new(&settings.select)?;
    let mut selected: Vec<Arc<Repo>> = Vec::new();
    let mut excluded = 0usize;
    for r in &disc.repos {
        match filter.check(r) {
            Ok(()) => selected.push(Arc::new(r.clone())),
            Err(reason) => {
                excluded += 1;
                ui.debug(format!("  {} {:<9} {}  ({reason})", style("-").dim(), "excluded", r.full_name));
            }
        }
    }

    // ---- plan -------------------------------------------------------------
    let fp = fingerprint(&settings);
    let mut jobs: Vec<Job> = Vec::new();
    let mut results: Vec<JobResult> = Vec::new();
    for repo in &selected {
        let kinds: &[Kind] = if settings.wikis && repo.has_wiki {
            &[Kind::Repo, Kind::Wiki]
        } else {
            &[Kind::Repo]
        };
        for &kind in kinds {
            let rel = rel_path(repo, kind, settings.mode);
            let path = root.join(&rel);
            let mut exists = path.exists();
            let mut prev = state.entries.get(&rel).cloned();

            if !exists
                && let Some(old) = find_renamed(&state, repo, kind, settings.mode, &rel, &root) {
                    if dry {
                        ui.info(format!("  would move {old} -> {rel} (renamed on GitHub)"));
                        exists = true;
                    } else if move_dir(&root.join(&old), &path).is_ok() {
                        ui.info(format!("  {} {:<9} {old} -> {rel}", style("↪").cyan(), "renamed"));
                        if let Some(mut e) = state.entries.remove(&old) {
                            e.pushed_at = None; // force a fetch at the new location
                            state.entries.insert(rel.clone(), e.clone());
                            prev = Some(e);
                        }
                        exists = true;
                    }
                }

            let job = Job {
                repo: repo.clone(),
                kind,
                url: remote_url(repo, kind, settings.transport),
                fingerprint: fp.clone(),
                action: Action::Clone,
                refresh: false,
                rel,
            };
            match decide(kind, exists, prev.as_ref(), repo, &fp, settings.force) {
                Decision::Run(action, refresh) => jobs.push(Job { action, refresh, ..job }),
                Decision::Skip(outcome) => {
                    print_result(&ui, &job, &outcome, None);
                    results.push(job_result(&job, &outcome, Duration::ZERO));
                }
            }
        }
    }

    let clones = jobs.iter().filter(|j| j.action == Action::Clone).count();
    let up_to_date = results.iter().filter(|r| r.status == Status::Unchanged).count();
    ui.info(format!(
        "{} selected ({excluded} excluded): {clones} to clone, {} to fetch, {up_to_date} up to date",
        plural(selected.len(), "repository", "repositories"),
        jobs.len() - clones,
    ));

    if dry {
        for job in &jobs {
            let verb = match job.action {
                Action::Clone => "would clone",
                Action::Update => "would fetch",
            };
            ui.always(format!("  {verb:<12} {}", job.label()));
        }
        let orphans = find_orphans(&state, &disc, &root);
        report_orphans(&ui, &settings, &orphans, true);
        return Ok(if disc.errors.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    // ---- execute ----------------------------------------------------------
    let cancel = CancellationToken::new();
    let env = auth::git_env(&settings, &access.host, access.token.as_ref())?;
    let ctx = Arc::new(Ctx {
        git: Git::new(env, settings.timeout, cancel.clone()),
        root: root.clone(),
        mode: settings.mode,
        pull_refs: settings.pull_refs,
        lfs: settings.lfs,
        retries: settings.retries,
    });

    // Largest first: the long clones start early and the run finishes sooner.
    jobs.sort_by_key(|j| std::cmp::Reverse(j.repo.size));
    let main = ui.bar(jobs.len() as u64, "syncing");
    let mut stream = futures::stream::iter(jobs.into_iter().map(|job| {
        let ctx = ctx.clone();
        let ui = ui.clone();
        let main = main.clone();
        async move {
            let pb = ui.spinner(&main, &job.label());
            let t = Instant::now();
            let outcome = ops::execute(&ctx, &job, &pb).await;
            ui.remove(&pb);
            (job, outcome, t.elapsed())
        }
    }))
    .buffer_unordered(settings.jobs);

    let sig = shutdown_signal();
    tokio::pin!(sig);
    let mut interrupted = false;
    let mut last_save = Instant::now();
    loop {
        tokio::select! {
            item = stream.next() => {
                let Some((job, outcome, elapsed)) = item else { break };
                main.inc(1);
                record(&mut state, &job, &outcome, settings.mode);
                print_result(&ui, &job, &outcome, Some(elapsed));
                results.push(job_result(&job, &outcome, elapsed));
                if let Some(store) = &store
                    && last_save.elapsed() > Duration::from_secs(30) {
                        store.save(&state)?;
                        last_save = Instant::now();
                    }
            }
            _ = &mut sig, if !interrupted => {
                interrupted = true;
                cancel.cancel();
                ui.warn("interrupted; stopping running git commands and saving state...");
            }
        }
    }
    ui.remove(&main);
    drop(stream);

    // ---- orphans ----------------------------------------------------------
    let mut orphans = Vec::new();
    if !interrupted && disc.errors.is_empty() {
        orphans = find_orphans(&state, &disc, &root);
        // Entries for vanished repositories whose directory is already gone are forgotten.
        let ids: HashSet<u64> = disc.repos.iter().map(|r| r.id).collect();
        state.entries.retain(|k, e| ids.contains(&e.id) || root.join(k).exists());
        orphans = apply_orphans(&ui, &settings, &root, &mut state, orphans);
    } else if !disc.errors.is_empty() {
        ui.warn("skipping orphan detection because the repository list may be incomplete");
    }

    // ---- save, archive, summary --------------------------------------------
    let counts = Counts::from(&results);
    state.last_run = Some(RunSummary {
        finished_at: Some(Utc::now()),
        cloned: counts.cloned,
        updated: counts.updated,
        unchanged: counts.unchanged,
        skipped: counts.skipped,
        failed: counts.failed,
    });
    if let Some(store) = &store {
        store.save(&state)?;
    }

    let mut archive_failed = false;
    let mut archive_path = None;
    if settings.archive.enabled && !interrupted {
        let archive_settings = settings.clone();
        let (root2, ui2) = (root.clone(), ui.clone());
        let res = tokio::task::spawn_blocking(move || crate::archive::create(&root2, &archive_settings, &ui2))
            .await
            .context("archive task panicked")?;
        match res {
            Ok(p) => archive_path = Some(p.display().to_string()),
            Err(e) => {
                ui.error(format!("archive failed: {e:#}"));
                archive_failed = true;
            }
        }
    }

    print_summary(&ui, &results, &counts, selected.len(), excluded, t0.elapsed());

    if let Some(path) = &settings.report {
        let report = Report {
            started_at,
            finished_at: Utc::now(),
            duration_secs: t0.elapsed().as_secs_f64(),
            dest: &root,
            mode: settings.mode,
            transport: settings.transport,
            dry_run: dry,
            interrupted,
            discovered: disc.repos.len(),
            selected: selected.len(),
            excluded,
            discovery_errors: &disc.errors,
            counts,
            orphans: &orphans,
            archive: archive_path,
            repos: &results,
        };
        write_report(path, &report)?;
    }

    Ok(if interrupted {
        ExitCode::from(130)
    } else if results.iter().any(|r| r.status == Status::Failed) || !disc.errors.is_empty() || archive_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn preflight(settings: &Settings) -> Result<()> {
    let git = std::process::Command::new("git").arg("--version").output();
    match git {
        Ok(o) if o.status.success() => {}
        _ => bail!("git is not installed or not on PATH"),
    }
    if settings.lfs && !settings.dry_run {
        let ok = std::process::Command::new("git")
            .args(["lfs", "version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            bail!("--lfs needs git-lfs (https://git-lfs.com)");
        }
    }
    Ok(())
}

fn record(state: &mut State, job: &Job, outcome: &Outcome, mode: Mode) {
    let now = Utc::now();
    match outcome.status {
        Status::Cloned | Status::Updated | Status::Unchanged | Status::Skipped => {
            state.entries.insert(
                job.rel.clone(),
                Entry {
                    id: job.repo.id,
                    full_name: job.repo.full_name.clone(),
                    kind: job.kind,
                    mode,
                    fingerprint: job.fingerprint.clone(),
                    pushed_at: job.repo.pushed_at.clone(),
                    last_success: Some(now),
                    last_error: None,
                    absent: outcome.absent,
                },
            );
        }
        Status::Failed => {
            let e = state.entries.entry(job.rel.clone()).or_insert_with(|| Entry {
                id: job.repo.id,
                full_name: job.repo.full_name.clone(),
                kind: job.kind,
                mode,
                fingerprint: job.fingerprint.clone(),
                pushed_at: None,
                last_success: None,
                last_error: None,
                absent: false,
            });
            e.last_error = outcome.detail.clone().or_else(|| Some("failed".into()));
        }
        Status::Cancelled => {}
    }
}

fn job_result(job: &Job, o: &Outcome, elapsed: Duration) -> JobResult {
    JobResult {
        name: job.repo.full_name.clone(),
        kind: job.kind,
        path: job.rel.clone(),
        status: o.status,
        detail: o.detail.clone(),
        warnings: o.warnings.clone(),
        seconds: elapsed.as_secs_f64(),
    }
}

fn print_result(ui: &Ui, job: &Job, o: &Outcome, elapsed: Option<Duration>) {
    let label = job.label();
    let time = elapsed
        .map(|d| format!("  {}", style(fmt_duration(d)).dim()))
        .unwrap_or_default();
    let detail = o
        .detail
        .as_deref()
        .map(|d| format!("  {}", style(format!("({d})")).dim()))
        .unwrap_or_default();
    match o.status {
        Status::Cloned => ui.info(format!("  {} {:<9} {label}{time}", style("✓").green(), "cloned")),
        Status::Updated => ui.info(format!("  {} {:<9} {label}{detail}{time}", style("↻").cyan(), "updated")),
        Status::Unchanged => ui.detail(format!("  {} {:<9} {label}{detail}", style("·").dim(), "unchanged")),
        Status::Skipped => ui.detail(format!("  {} {:<9} {label}{detail}", style("-").dim(), "skipped")),
        Status::Cancelled => ui.detail(format!("  {} {:<9} {label}", style("-").dim(), "cancelled")),
        Status::Failed => ui.always(format!(
            "  {} {:<9} {label}: {}",
            style("✗").red(),
            "failed",
            o.detail.as_deref().unwrap_or("unknown error")
        )),
    }
    for w in &o.warnings {
        ui.warn(format!("{label}: {w}"));
    }
}

fn print_summary(ui: &Ui, results: &[JobResult], c: &Counts, selected: usize, excluded: usize, took: Duration) {
    let mut parts = vec![
        format!("{} cloned", c.cloned),
        format!("{} updated", c.updated),
        format!("{} unchanged", c.unchanged),
    ];
    if c.skipped > 0 {
        parts.push(format!("{} skipped", c.skipped));
    }
    let failed = format!("{} failed", c.failed);
    parts.push(if c.failed > 0 {
        style(failed).red().bold().to_string()
    } else {
        failed
    });
    if c.cancelled > 0 {
        parts.push(format!("{} cancelled", c.cancelled));
    }
    ui.always(format!(
        "{} {} ({} excluded) in {}: {}",
        style("Done:").bold(),
        plural(selected, "repository", "repositories"),
        excluded,
        fmt_duration(took),
        parts.join(", ")
    ));
    let failures: Vec<&JobResult> = results.iter().filter(|r| r.status == Status::Failed).collect();
    if !failures.is_empty() {
        ui.always(style("Failures:").red().bold().to_string());
        for f in failures {
            ui.always(format!("  {}: {}", f.path, f.detail.as_deref().unwrap_or("")));
        }
    }
}

/// Backed-up repositories (with a directory on disk) that GitHub no longer
/// returns from any configured source. Repositories that are merely filtered
/// out are not orphans; they are left alone.
fn find_orphans(state: &State, disc: &Discovered, root: &Path) -> Vec<String> {
    if !disc.errors.is_empty() {
        return Vec::new();
    }
    let ids: HashSet<u64> = disc.repos.iter().map(|r| r.id).collect();
    state
        .entries
        .iter()
        .filter(|(k, e)| !ids.contains(&e.id) && root.join(k).exists())
        .map(|(k, _)| k.clone())
        .collect()
}

fn report_orphans(ui: &Ui, settings: &Settings, orphans: &[String], dry: bool) {
    if orphans.is_empty() {
        return;
    }
    let verb = match (settings.orphans, dry) {
        (Orphans::Keep, _) => "kept",
        (Orphans::Move, true) => "would move",
        (Orphans::Delete, true) => "would delete",
        (Orphans::Move, false) => "moved",
        (Orphans::Delete, false) => "deleted",
    };
    ui.always(format!(
        "{} no longer on GitHub ({verb}):",
        style(format!(
            "{} backed up here",
            plural(orphans.len(), "repository", "repositories")
        ))
        .yellow()
    ));
    for o in orphans.iter().take(20) {
        ui.always(format!("  {o}"));
    }
    if orphans.len() > 20 {
        ui.always(format!("  ... and {} more", orphans.len() - 20));
    }
}

fn apply_orphans(ui: &Ui, settings: &Settings, root: &Path, state: &mut State, orphans: Vec<String>) -> Vec<String> {
    if orphans.is_empty() {
        return orphans;
    }
    let mut policy = settings.orphans;
    let managed = state.entries.len().max(1);
    if policy != Orphans::Keep && orphans.len() > 5 && orphans.len() * 5 > managed && !settings.force {
        ui.warn(format!(
            "{} of {} backed-up repositories disappeared from GitHub; not applying orphans = {} \
             as a safety measure. Re-run with --force if this is expected.",
            orphans.len(),
            managed,
            format!("{:?}", settings.orphans).to_lowercase()
        ));
        policy = Orphans::Keep;
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut done = Vec::new();
    for key in orphans {
        let path = root.join(&key);
        let res = match policy {
            Orphans::Keep => Ok(()),
            Orphans::Move => move_dir(&path, &root.join(ORPHAN_DIR).join(&stamp).join(&key)),
            Orphans::Delete => std::fs::remove_dir_all(&path).map_err(Into::into),
        };
        match res {
            Ok(()) => {
                if policy != Orphans::Keep {
                    state.entries.remove(&key);
                }
                done.push(key);
            }
            Err(e) => ui.error(format!("{key}: {e:#}")),
        }
    }
    let shown = Settings {
        orphans: policy,
        ..settings.clone()
    };
    report_orphans(ui, &shown, &done, false);
    done
}

fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    std::fs::rename(from, to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    // Remove the owner directory if it is now empty.
    if let Some(parent) = from.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

fn write_report(path: &PathBuf, report: &Report) -> Result<()> {
    let json = serde_json::to_vec_pretty(report)?;
    std::fs::write(path, json).with_context(|| format!("writing report {}", path.display()))
}

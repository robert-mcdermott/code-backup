use crate::config::Settings;
use crate::git::Git;
use crate::sync::shutdown_signal;
use crate::ui::Ui;
use crate::util::{fmt_duration, plural};
use anyhow::{Result, bail};
use console::style;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Run `git fsck` on every repository in the backup directory.
pub async fn run(settings: &Settings, ui: &Ui, full: bool, jobs: Option<usize>) -> Result<ExitCode> {
    let root = settings.require_dest()?.to_path_buf();
    if !root.is_dir() {
        bail!("backup directory {} does not exist", root.display());
    }
    let repos = find_repos(&root)?;
    if repos.is_empty() {
        ui.always(format!("No repositories found in {}", root.display()));
        return Ok(ExitCode::SUCCESS);
    }
    let jobs = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
        .max(1);
    ui.info(format!(
        "Checking {} ({} check, {} at a time)...",
        plural(repos.len(), "repository", "repositories"),
        if full { "full" } else { "connectivity" },
        jobs
    ));

    let t0 = Instant::now();
    let cancel = CancellationToken::new();
    let git = Git::new(
        vec![("LC_ALL".into(), "C".into())],
        Duration::from_secs(6 * 3600),
        cancel.clone(),
    );
    let main = ui.bar(repos.len() as u64, "verifying");
    let mut stream = futures::stream::iter(repos.iter().map(|path| {
        let git = git.clone();
        let ui = ui.clone();
        let main = main.clone();
        let label = path.strip_prefix(&root).unwrap_or(path).display().to_string();
        async move {
            let pb = ui.spinner(&main, &label);
            pb.set_message("git fsck");
            let mut args = vec!["fsck", "--no-progress", "--no-dangling"];
            if !full {
                args.push("--connectivity-only");
            }
            let res = git.run(Some(path), &args).await;
            ui.remove(&pb);
            (label, res)
        }
    }))
    .buffer_unordered(jobs);

    let sig = shutdown_signal();
    tokio::pin!(sig);
    let mut bad = 0usize;
    let mut checked = 0usize;
    let mut interrupted = false;
    loop {
        tokio::select! {
            item = stream.next() => {
                let Some((label, res)) = item else { break };
                main.inc(1);
                match res {
                    Ok(_) => {
                        checked += 1;
                        ui.detail(format!("  {} {label}", style("✓").green()));
                    }
                    Err(crate::git::GitError::Cancelled) => {}
                    Err(e) => {
                        checked += 1;
                        bad += 1;
                        ui.always(format!("  {} {label}: {e}", style("✗").red()));
                    }
                }
            }
            _ = &mut sig, if !interrupted => {
                interrupted = true;
                cancel.cancel();
                ui.warn("interrupted");
            }
        }
    }
    ui.remove(&main);

    ui.always(format!(
        "{} checked {} in {}: {}",
        style("Verify:").bold(),
        plural(checked, "repository", "repositories"),
        fmt_duration(t0.elapsed()),
        if bad == 0 {
            style("no problems found".to_string()).green().to_string()
        } else {
            style(format!("{bad} with problems")).red().bold().to_string()
        }
    ));
    Ok(if interrupted {
        ExitCode::from(130)
    } else if bad > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// `<root>/<owner>/<repo>` directories that look like git repositories
/// (bare mirrors or working trees). Hidden directories are skipped.
fn find_repos(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for owner in sorted_dirs(root)? {
        for repo in sorted_dirs(&owner)? {
            let name = repo.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".partial") {
                continue;
            }
            let bare = repo.join("HEAD").is_file() && repo.join("objects").is_dir();
            if bare || repo.join(".git").exists() {
                out.push(repo);
            }
        }
    }
    Ok(out)
}

fn sorted_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    v.sort();
    Ok(v)
}

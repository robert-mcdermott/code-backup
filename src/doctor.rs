use crate::auth::resolve_token;
use crate::config::{Discovery, Settings, Transport};
use crate::discover::ApiClient;
use crate::state::read_state_for;
use crate::ui::Ui;
use crate::util::{git_host_from_api, which};
use anyhow::Result;
use console::style;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

struct Checks<'a> {
    ui: &'a Ui,
    problems: usize,
}

impl Checks<'_> {
    fn ok(&self, msg: impl AsRef<str>) {
        self.ui.always(format!("  {} {}", style("✓").green(), msg.as_ref()));
    }
    fn note(&self, msg: impl AsRef<str>) {
        self.ui.always(format!("  {} {}", style("·").dim(), msg.as_ref()));
    }
    fn warn(&self, msg: impl AsRef<str>) {
        self.ui.always(format!("  {} {}", style("!").yellow(), msg.as_ref()));
    }
    fn fail(&mut self, msg: impl AsRef<str>) {
        self.problems += 1;
        self.ui.always(format!("  {} {}", style("✗").red(), msg.as_ref()));
    }
}

async fn capture(cmd: &mut tokio::process::Command) -> Option<std::process::Output> {
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()
}

fn last_line(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

pub async fn run(settings: &Settings, ui: &Ui) -> Result<ExitCode> {
    let mut c = Checks { ui, problems: 0 };
    let host = git_host_from_api(&settings.api_url);
    ui.always(format!(
        "{} discovery={:?} transport={:?} mode={:?} host={host}",
        style("code-backup doctor").bold(),
        settings.discovery,
        settings.transport,
        settings.mode
    ).to_lowercase());

    match &settings.config_path {
        Some(p) => c.ok(format!("config file {}", p.display())),
        None => c.note("no config file; using defaults and flags (create one with `code-backup init`)"),
    }

    // git
    match capture(tokio::process::Command::new("git").arg("--version")).await {
        Some(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            let version: Vec<u32> = text
                .split_whitespace()
                .nth(2)
                .unwrap_or("")
                .split('.')
                .take(2)
                .filter_map(|p| p.parse().ok())
                .collect();
            if version.len() == 2 && (version[0], version[1]) >= (2, 31) {
                c.ok(text);
            } else {
                c.fail(format!("{text}: git 2.31 or newer is required"));
            }
        }
        _ => c.fail("git not found on PATH"),
    }

    // git-lfs
    match capture(tokio::process::Command::new("git").args(["lfs", "version"])).await {
        Some(o) if o.status.success() => c.ok(String::from_utf8_lossy(&o.stdout).trim()),
        _ if settings.lfs => c.fail("git-lfs is not installed but lfs = true"),
        _ => c.note("git-lfs not installed (only needed with --lfs)"),
    }

    // gh
    let gh_needed = settings.discovery == Discovery::Gh || settings.transport == Transport::Gh;
    match which("gh") {
        Some(gh) => {
            let status = capture(
                tokio::process::Command::new(&gh)
                    .args(["auth", "status", "--hostname", &host])
                    .env("GH_NO_UPDATE_NOTIFIER", "1"),
            )
            .await;
            match status {
                Some(o) if o.status.success() => c.ok(format!("gh is logged in to {host}")),
                _ if gh_needed => c.fail(format!("gh is not logged in to {host}; run `gh auth login`")),
                _ => c.warn(format!("gh is installed but not logged in to {host}")),
            }
        }
        None if gh_needed => c.fail("gh CLI not found on PATH (https://cli.github.com)"),
        None => c.note("gh CLI not installed (only needed for discovery/transport gh)"),
    }

    // token + API
    let token_needed = settings.discovery == Discovery::Api || settings.transport == Transport::Https;
    match resolve_token(settings, &host).await {
        Ok(Some(t)) => match ApiClient::new(&settings.api_url, Some(t.value.clone()))?.user_info().await {
            Ok(u) => {
                c.ok(format!(
                    "token from {} works: authenticated as {}{}",
                    t.source,
                    u.login,
                    u.rate_remaining
                        .map(|r| format!(" ({r} API requests left this hour)"))
                        .unwrap_or_default()
                ));
                if let Some(scopes) = u.scopes {
                    let has_repo = scopes.split(',').any(|s| s.trim() == "repo");
                    if has_repo {
                        c.ok(format!("token scopes: {scopes}"));
                    } else {
                        c.warn(format!(
                            "token scopes: {scopes:?}; without `repo`, private repositories are not listed"
                        ));
                    }
                }
            }
            Err(e) => c.fail(format!("token from {} was rejected: {e:#}", t.source)),
        },
        Ok(None) if token_needed => c.fail(
            "no GitHub token found: set GITHUB_TOKEN, configure token_command, or run `gh auth login`",
        ),
        Ok(None) => c.note("no GitHub token found (not needed for this configuration)"),
        Err(e) => c.fail(format!("token lookup failed: {e:#}")),
    }

    // SSH
    if settings.transport == Transport::Ssh || settings.ssh_key.is_some() {
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
        if let Some(key) = &settings.ssh_key {
            cmd.args(["-o", "IdentitiesOnly=yes", "-i"]).arg(key);
        }
        cmd.arg(format!("git@{host}"));
        match capture(&mut cmd).await {
            Some(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                if let Some(line) = err.lines().find(|l| l.contains("successfully authenticated")) {
                    c.ok(format!("ssh: {}", line.trim()));
                } else {
                    c.fail(format!("ssh to git@{host} failed: {}", last_line(&o.stderr)));
                }
            }
            None => c.fail(format!("ssh to git@{host} did not respond")),
        }
    } else {
        c.note("ssh not checked (transport is not ssh)");
    }

    // Backup directory
    match &settings.dest {
        Some(dest) if dest.is_dir() => {
            let probe = dest.join(".code-backup-write-test");
            match std::fs::write(&probe, b"ok") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    c.ok(format!("backup directory {} is writable", dest.display()));
                }
                Err(e) => c.fail(format!("cannot write to {}: {e}", dest.display())),
            }
            match read_state_for(dest) {
                Ok(state) => {
                    let failing = state.entries.values().filter(|e| e.last_error.is_some()).count();
                    let last = state
                        .last_run
                        .as_ref()
                        .and_then(|r| r.finished_at)
                        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".into());
                    let msg = format!(
                        "{} repositories tracked, last sync: {last}, {failing} failing",
                        state.entries.len()
                    );
                    if failing > 0 { c.warn(msg) } else { c.ok(msg) }
                }
                Err(e) => c.fail(format!("state file unreadable: {e:#}")),
            }
        }
        Some(dest) => c.note(format!("backup directory {} will be created on first sync", dest.display())),
        None => c.warn("no backup directory configured (set `dest` or pass --dest)"),
    }

    if c.problems == 0 {
        ui.always(style("All checks passed.").green().to_string());
        Ok(ExitCode::SUCCESS)
    } else {
        ui.always(style(format!("{} problem(s) found.", c.problems)).red().to_string());
        Ok(ExitCode::from(1))
    }
}

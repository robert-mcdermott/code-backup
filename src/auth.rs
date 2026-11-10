use crate::config::{Settings, Transport};
use crate::util::{shell_quote, which};
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

pub struct Token {
    pub value: String,
    pub source: String,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("value", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// Find a GitHub token: `token_command`, `$GITHUB_TOKEN`, `$GH_TOKEN`, then `gh auth token`.
pub async fn resolve_token(settings: &Settings, host: &str) -> Result<Option<Token>> {
    if let Some(cmd) = &settings.token_command {
        let out = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .await
            .context("running token_command")?;
        if !out.status.success() {
            bail!("token_command exited with {}", out.status);
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if value.is_empty() {
            bail!("token_command printed nothing");
        }
        return Ok(Some(Token {
            value,
            source: "token_command".into(),
        }));
    }
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Ok(Some(Token {
                    value: v.to_owned(),
                    source: format!("${var}"),
                }));
            }
        }
    }
    if let Some(gh) = which("gh") {
        let out = tokio::process::Command::new(gh)
            .args(["auth", "token", "--hostname", host])
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await;
        if let Ok(out) = out {
            let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if out.status.success() && !value.is_empty() {
                return Ok(Some(Token {
                    value,
                    source: "gh auth token".into(),
                }));
            }
        }
    }
    Ok(None)
}

/// Environment for every git child process. Secrets go through environment
/// variables (GIT_CONFIG_*), never through command-line arguments or remote
/// URLs, so they do not show up in `ps` output or in `.git/config`.
pub fn git_env(
    settings: &Settings,
    host: &str,
    token: Option<&Token>,
) -> Result<Vec<(OsString, OsString)>> {
    let mut env: Vec<(OsString, OsString)> = vec![
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GCM_INTERACTIVE".into(), "never".into()),
        ("LC_ALL".into(), "C".into()),
    ];
    if !settings.lfs {
        env.push(("GIT_LFS_SKIP_SMUDGE".into(), "1".into()));
    }

    let mut config: Vec<(String, String)> = Vec::new();
    match settings.transport {
        Transport::Ssh => {
            if let Some(cmd) = ssh_command(settings.ssh_key.as_deref()) {
                env.push(("GIT_SSH_COMMAND".into(), cmd.into()));
            }
        }
        Transport::Https => {
            let token = token.context(
                "--transport https needs a GitHub token: set GITHUB_TOKEN, configure token_command, \
                 or log in with `gh auth login`",
            )?;
            let basic = base64::engine::general_purpose::STANDARD
                .encode(format!("x-access-token:{}", token.value));
            config.push((
                format!("http.https://{host}/.extraheader"),
                format!("Authorization: Basic {basic}"),
            ));
        }
        Transport::Gh => {
            let gh = which("gh").context("--transport gh needs the gh CLI on PATH")?;
            let key = format!("credential.https://{host}.helper");
            // An empty value resets any helpers from the user's git config.
            config.push((key.clone(), String::new()));
            config.push((
                key,
                format!("!{} auth git-credential", shell_quote(&gh.to_string_lossy())),
            ));
        }
    }

    if !config.is_empty() {
        // Append to any GIT_CONFIG_COUNT the user already has.
        let base: usize = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        for (i, (k, v)) in config.iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{}", base + i).into(), k.into()));
            env.push((format!("GIT_CONFIG_VALUE_{}", base + i).into(), v.into()));
        }
        env.push((
            "GIT_CONFIG_COUNT".into(),
            (base + config.len()).to_string().into(),
        ));
    }
    Ok(env)
}

/// SSH command for git. An explicit key always wins. Otherwise only add
/// BatchMode (no interactive passphrase prompts from parallel jobs) when the
/// user has not configured their own ssh command.
fn ssh_command(key: Option<&Path>) -> Option<String> {
    if let Some(key) = key {
        return Some(format!(
            "ssh -o BatchMode=yes -o IdentitiesOnly=yes -i {}",
            shell_quote(&key.to_string_lossy())
        ));
    }
    if std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some() {
        return None;
    }
    let configured = std::process::Command::new("git")
        .args(["config", "--get", "core.sshCommand"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if configured {
        None
    } else {
        Some("ssh -o BatchMode=yes".into())
    }
}

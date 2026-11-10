use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Runs `git` child processes with a shared environment, a per-command
/// timeout and cooperative cancellation (Ctrl-C / SIGTERM).
#[derive(Clone)]
pub struct Git {
    env: Arc<Vec<(OsString, OsString)>>,
    timeout: Duration,
    cancel: CancellationToken,
}

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub enum GitError {
    Failed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    Timeout(Duration),
    Cancelled,
    Io(String),
}

impl GitError {
    pub fn io(context: &str, e: std::io::Error) -> Self {
        GitError::Io(format!("{context}: {e}"))
    }

    pub fn stderr(&self) -> &str {
        match self {
            GitError::Failed { stderr, .. } => stderr,
            _ => "",
        }
    }

    /// "Repository not found" style errors (missing repo, missing wiki, no access).
    pub fn is_not_found(&self) -> bool {
        let s = self.stderr().to_ascii_lowercase();
        s.contains("not found") || s.contains("does not appear to be a git repository")
    }

    /// Network hiccups are worth retrying; auth and missing repositories are not.
    pub fn is_retryable(&self) -> bool {
        let GitError::Failed { stderr, .. } = self else {
            return false;
        };
        const PERMANENT: [&str; 12] = [
            "not found",
            "permission denied",
            "authentication failed",
            "could not read username",
            "could not read password",
            "terminal prompts disabled",
            "access denied",
            "invalid username or password",
            "does not appear to be a git repository",
            "host key verification failed",
            "access to this repository has been disabled",
            "write access to repository not granted",
        ];
        let s = stderr.to_ascii_lowercase();
        !PERMANENT.iter().any(|p| s.contains(p))
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Failed { cmd, code, stderr } => {
                let summary = summarize(stderr);
                match (summary.is_empty(), code) {
                    (false, _) => write!(f, "git {cmd}: {summary}"),
                    (true, Some(c)) => write!(f, "git {cmd} exited with code {c}"),
                    (true, None) => write!(f, "git {cmd} was killed by a signal"),
                }
            }
            GitError::Timeout(d) => write!(f, "timed out after {}", humantime::format_duration(*d)),
            GitError::Cancelled => write!(f, "cancelled"),
            GitError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

/// The useful part of git's stderr: the last few non-hint lines.
fn summarize(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
        .collect();
    let start = lines.len().saturating_sub(3);
    lines[start..].join(" | ")
}

impl Git {
    pub fn new(env: Vec<(OsString, OsString)>, timeout: Duration, cancel: CancellationToken) -> Self {
        Git {
            env: Arc::new(env),
            timeout,
            cancel,
        }
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Run git and return its output whatever the exit code.
    pub async fn output(&self, dir: Option<&Path>, args: &[&str]) -> Result<Output, GitError> {
        if self.cancel.is_cancelled() {
            return Err(GitError::Cancelled);
        }
        let mut cmd = tokio::process::Command::new("git");
        if let Some(d) = dir {
            cmd.arg("-C").arg(d);
        }
        cmd.args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group: terminal Ctrl-C reaches only us, and we can stop
        // git together with its helpers (ssh, git-remote-https, index-pack).
        #[cfg(unix)]
        cmd.process_group(0);

        tracing::debug!(dir = ?dir, "git {}", args.join(" "));
        let child = cmd
            .spawn()
            .map_err(|e| GitError::io("failed to start git (is it installed and on PATH?)", e))?;
        let pid = child.id();
        let wait = child.wait_with_output();
        tokio::pin!(wait);

        let stop = tokio::select! {
            res = &mut wait => {
                let out = res.map_err(|e| GitError::io("waiting for git", e))?;
                return Ok(Output {
                    code: out.status.code(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                });
            }
            _ = tokio::time::sleep(self.timeout) => GitError::Timeout(self.timeout),
            _ = self.cancel.cancelled() => GitError::Cancelled,
        };

        // SIGTERM first so git can remove its lock files, then SIGKILL.
        signal_group(pid, Sig::Term);
        if tokio::time::timeout(Duration::from_secs(3), &mut wait).await.is_err() {
            signal_group(pid, Sig::Kill);
        }
        Err(stop)
    }

    /// Run git and fail on a non-zero exit code.
    pub async fn run(&self, dir: Option<&Path>, args: &[&str]) -> Result<Output, GitError> {
        let out = self.output(dir, args).await?;
        if out.code == Some(0) {
            Ok(out)
        } else {
            Err(GitError::Failed {
                cmd: args.first().copied().unwrap_or("").to_owned(),
                code: out.code,
                stderr: out.stderr,
            })
        }
    }
}

enum Sig {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, sig: Sig) {
    let Some(pid) = pid else { return };
    let sig = match sig {
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
    };
    // SAFETY: plain syscall; the group id is the child's pid (process_group(0)).
    unsafe {
        libc::killpg(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _sig: Sig) {
    // kill_on_drop handles the direct child on other platforms.
}

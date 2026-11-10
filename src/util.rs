use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Expand a leading `~` or `~/` to the user's home directory.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~"
        && let Some(h) = home_dir() {
            return h;
        }
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(h) = home_dir() {
            return h.join(rest);
        }
    PathBuf::from(p)
}

/// True if `s` is safe to use as a single path component and URL segment.
/// GitHub owner and repository names only use these characters, so anything
/// else (including `..`) is rejected rather than written to disk.
pub fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.len() <= 255
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Quote a string for POSIX `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Locate an executable on `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// `foo.git` -> `foo.git.partial` (same directory).
pub fn partial_path(p: &Path) -> PathBuf {
    let mut name: OsString = p.file_name().unwrap_or_default().to_owned();
    name.push(".partial");
    p.with_file_name(name)
}

/// `plural(1, "repository", "repositories")` -> "1 repository"
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    } else {
        format!("{}h{:02}m", d.as_secs() / 3600, (d.as_secs() % 3600) / 60)
    }
}

/// Host used for git operations, derived from the API base URL.
/// `https://api.github.com` -> `github.com`, `https://ghe.corp/api/v3` -> `ghe.corp`.
pub fn git_host_from_api(api_url: &str) -> String {
    let host = reqwest::Url::parse(api_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "github.com".to_owned());
    host.strip_prefix("api.").map(str::to_owned).unwrap_or(host)
}

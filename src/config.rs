use crate::cli::{ArchiveOpts, ConnArgs, DestArgs, SelectArgs, SyncOpts, flag};
use crate::util::{expand_tilde, home_dir};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Bare `git clone --mirror` style repositories with every branch and tag
    Mirror,
    /// Normal clones with a working tree, fast-forwarded when safe
    Checkout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// git@github.com:owner/repo.git using your SSH agent or --ssh-key
    Ssh,
    /// https:// URLs authenticated with a GitHub token
    Https,
    /// https:// URLs authenticated through `gh auth git-credential`
    Gh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Discovery {
    /// Call the GitHub REST API directly with a token
    Api,
    /// Use `gh api` (reuses the gh CLI login)
    Gh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Orphans {
    /// Leave local copies of repositories that disappeared from GitHub
    Keep,
    /// Move them to <dest>/.orphaned/<timestamp>/
    Move,
    /// Delete them
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    All,
    Public,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
pub enum ArchiveFormat {
    #[serde(rename = "tar.zst")]
    #[value(name = "tar.zst")]
    TarZst,
    #[serde(rename = "tar.gz")]
    #[value(name = "tar.gz")]
    TarGz,
    #[serde(rename = "tar")]
    #[value(name = "tar")]
    Tar,
}

impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::TarZst => "tar.zst",
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::Tar => "tar",
        }
    }
}

// ---------------------------------------------------------------------------
// Config file
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub dest: Option<String>,
    pub mode: Option<Mode>,
    pub transport: Option<Transport>,
    pub discovery: Option<Discovery>,
    pub jobs: Option<usize>,
    pub ssh_key: Option<String>,
    pub token_command: Option<String>,
    pub api_url: Option<String>,
    pub lfs: Option<bool>,
    pub wikis: Option<bool>,
    pub pull_refs: Option<bool>,
    pub orphans: Option<Orphans>,
    pub retries: Option<u32>,
    pub timeout: Option<String>,
    pub select: FileSelect,
    pub archive: FileArchive,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileSelect {
    pub mine: Option<bool>,
    pub affiliation: Option<Vec<String>>,
    pub users: Option<Vec<String>>,
    pub orgs: Option<Vec<String>>,
    pub repos: Option<Vec<String>>,
    pub starred: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub forks: Option<bool>,
    pub archived: Option<bool>,
    pub visibility: Option<Visibility>,
    pub max_size_mb: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileArchive {
    pub enabled: Option<bool>,
    pub dir: Option<String>,
    pub format: Option<ArchiveFormat>,
    pub level: Option<i32>,
    pub keep: Option<usize>,
    pub prefix: Option<String>,
}

pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))?;
    Some(base.join("code-backup").join("config.toml"))
}

/// Load the config file. An explicitly requested file must exist; the default
/// location is optional.
pub fn load_file(explicit: Option<&Path>) -> Result<(FileConfig, Option<PathBuf>)> {
    let (path, required) = match explicit {
        Some(p) => (p.to_path_buf(), true),
        None => match default_config_path() {
            Some(p) => (p, false),
            None => return Ok((FileConfig::default(), None)),
        },
    };
    if !path.exists() {
        if required {
            bail!("config file {} does not exist", path.display());
        }
        return Ok((FileConfig::default(), None));
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let cfg: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
    Ok((cfg, Some(path)))
}

// ---------------------------------------------------------------------------
// Resolved settings (defaults <- config file <- command line)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Settings {
    pub config_path: Option<PathBuf>,
    pub dest: Option<PathBuf>,
    pub mode: Mode,
    pub transport: Transport,
    pub discovery: Discovery,
    pub jobs: usize,
    pub ssh_key: Option<PathBuf>,
    pub token_command: Option<String>,
    pub api_url: String,
    pub lfs: bool,
    pub wikis: bool,
    pub pull_refs: bool,
    pub orphans: Orphans,
    pub retries: u32,
    pub timeout: Duration,
    pub force: bool,
    pub dry_run: bool,
    pub report: Option<PathBuf>,
    pub select: Selection,
    pub archive: ArchiveSettings,
}

#[derive(Debug, Clone)]
pub struct Selection {
    mine: Option<bool>,
    pub affiliation: Vec<String>,
    pub users: Vec<String>,
    pub orgs: Vec<String>,
    pub repos: Vec<String>,
    pub starred: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub forks: bool,
    pub archived: bool,
    pub visibility: Visibility,
    pub max_size_mb: Option<u64>,
}

impl Selection {
    /// Your own repositories are selected unless turned off, or unless other
    /// sources were given without asking for them explicitly.
    pub fn mine(&self) -> bool {
        self.mine.unwrap_or(
            self.users.is_empty() && self.orgs.is_empty() && self.repos.is_empty() && !self.starred,
        )
    }
}

#[derive(Debug, Clone)]
pub struct ArchiveSettings {
    pub enabled: bool,
    pub dir: Option<PathBuf>,
    pub format: ArchiveFormat,
    pub level: i32,
    pub keep: usize,
    pub prefix: String,
}

const VALID_AFFILIATIONS: [&str; 3] = ["owner", "collaborator", "organization_member"];

fn parse_timeout(s: &str) -> Result<Duration> {
    humantime::parse_duration(s).with_context(|| format!("invalid timeout {s:?} (try \"30m\" or \"2h\")"))
}

impl Settings {
    pub fn from_file(file: FileConfig, config_path: Option<PathBuf>) -> Result<Self> {
        let s = file.select;
        let a = file.archive;
        Ok(Settings {
            config_path,
            dest: file.dest.as_deref().map(expand_tilde),
            mode: file.mode.unwrap_or(Mode::Mirror),
            transport: file.transport.unwrap_or(Transport::Ssh),
            discovery: file.discovery.unwrap_or(Discovery::Api),
            jobs: file.jobs.unwrap_or(8),
            ssh_key: file.ssh_key.as_deref().map(expand_tilde),
            token_command: file.token_command,
            api_url: file.api_url.unwrap_or_else(|| "https://api.github.com".into()),
            lfs: file.lfs.unwrap_or(false),
            wikis: file.wikis.unwrap_or(false),
            pull_refs: file.pull_refs.unwrap_or(false),
            orphans: file.orphans.unwrap_or(Orphans::Keep),
            retries: file.retries.unwrap_or(2),
            timeout: parse_timeout(file.timeout.as_deref().unwrap_or("60m"))?,
            force: false,
            dry_run: false,
            report: None,
            select: Selection {
                mine: s.mine,
                affiliation: s.affiliation.unwrap_or_else(|| vec!["owner".into()]),
                users: s.users.unwrap_or_default(),
                orgs: s.orgs.unwrap_or_default(),
                repos: s.repos.unwrap_or_default(),
                starred: s.starred.unwrap_or(false),
                include: s.include.unwrap_or_default(),
                exclude: s.exclude.unwrap_or_default(),
                forks: s.forks.unwrap_or(true),
                archived: s.archived.unwrap_or(true),
                visibility: s.visibility.unwrap_or(Visibility::All),
                max_size_mb: s.max_size_mb.filter(|&m| m > 0),
            },
            archive: ArchiveSettings {
                enabled: a.enabled.unwrap_or(false),
                dir: a.dir.as_deref().map(expand_tilde),
                format: a.format.unwrap_or(ArchiveFormat::TarZst),
                level: a.level.unwrap_or(3),
                keep: a.keep.unwrap_or(0),
                prefix: a.prefix.unwrap_or_else(|| "github-backup".into()),
            },
        })
    }

    pub fn apply_dest(&mut self, a: &DestArgs) {
        if let Some(d) = &a.dest {
            self.dest = Some(d.clone());
        }
    }

    pub fn apply_conn(&mut self, a: &ConnArgs) {
        if let Some(d) = a.discovery {
            self.discovery = d;
        }
        if let Some(u) = &a.api_url {
            self.api_url = u.clone();
        }
        if let Some(c) = &a.token_command {
            self.token_command = Some(c.clone());
        }
    }

    pub fn apply_select(&mut self, a: &SelectArgs) {
        let s = &mut self.select;
        if let Some(v) = flag(a.mine, a.no_mine) {
            s.mine = Some(v);
        }
        if let Some(v) = &a.affiliation {
            s.affiliation = v.clone();
        }
        if !a.user.is_empty() {
            s.users = a.user.clone();
        }
        if !a.org.is_empty() {
            s.orgs = a.org.clone();
        }
        if !a.repo.is_empty() {
            s.repos = a.repo.clone();
        }
        if let Some(v) = flag(a.starred, a.no_starred) {
            s.starred = v;
        }
        if !a.include.is_empty() {
            s.include = a.include.clone();
        }
        if !a.exclude.is_empty() {
            s.exclude = a.exclude.clone();
        }
        if let Some(v) = flag(a.forks, a.no_forks) {
            s.forks = v;
        }
        if let Some(v) = flag(a.archived, a.no_archived) {
            s.archived = v;
        }
        if let Some(v) = a.visibility {
            s.visibility = v;
        }
        if let Some(v) = a.max_size {
            s.max_size_mb = Some(v).filter(|&m| m > 0);
        }
    }

    pub fn apply_sync(&mut self, a: &SyncOpts) -> Result<()> {
        if let Some(v) = a.mode {
            self.mode = v;
        }
        if let Some(v) = a.transport {
            self.transport = v;
        }
        if let Some(v) = &a.ssh_key {
            self.ssh_key = Some(v.clone());
        }
        if let Some(v) = a.jobs {
            self.jobs = v;
        }
        if let Some(v) = flag(a.lfs, a.no_lfs) {
            self.lfs = v;
        }
        if let Some(v) = flag(a.wikis, a.no_wikis) {
            self.wikis = v;
        }
        if let Some(v) = flag(a.pull_refs, a.no_pull_refs) {
            self.pull_refs = v;
        }
        if let Some(v) = a.orphans {
            self.orphans = v;
        }
        if let Some(v) = a.retries {
            self.retries = v;
        }
        if let Some(v) = &a.timeout {
            self.timeout = parse_timeout(v)?;
        }
        if let Some(v) = flag(a.archive, a.no_archive) {
            self.archive.enabled = v;
        }
        self.force = a.force;
        self.dry_run = a.dry_run;
        self.report = a.report.clone();
        Ok(())
    }

    pub fn apply_archive(&mut self, a: &ArchiveOpts) {
        let s = &mut self.archive;
        if let Some(v) = &a.archive_dir {
            s.dir = Some(v.clone());
        }
        if let Some(v) = a.archive_format {
            s.format = v;
        }
        if let Some(v) = a.archive_level {
            s.level = v;
        }
        if let Some(v) = a.archive_keep {
            s.keep = v;
        }
        if let Some(v) = &a.archive_prefix {
            s.prefix = v.clone();
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.jobs == 0 {
            bail!("jobs must be at least 1");
        }
        for a in &self.select.affiliation {
            if !VALID_AFFILIATIONS.contains(&a.as_str()) {
                bail!(
                    "invalid affiliation {a:?}; expected one of: {}",
                    VALID_AFFILIATIONS.join(", ")
                );
            }
        }
        if self.select.affiliation.is_empty() {
            bail!("affiliation must list at least one of: {}", VALID_AFFILIATIONS.join(", "));
        }
        let level_ok = match self.archive.format {
            ArchiveFormat::TarZst => (1..=22).contains(&self.archive.level),
            ArchiveFormat::TarGz => (0..=9).contains(&self.archive.level),
            ArchiveFormat::Tar => true,
        };
        if !level_ok {
            bail!(
                "archive level {} is out of range for {} (zstd: 1-22, gzip: 0-9)",
                self.archive.level,
                self.archive.format.extension()
            );
        }
        if self.archive.prefix.is_empty() || !crate::util::is_safe_component(&self.archive.prefix) {
            bail!("archive prefix may only contain letters, digits, '-', '_' and '.'");
        }
        if let Some(k) = &self.ssh_key
            && !k.is_file() {
                bail!("ssh key {} does not exist", k.display());
            }
        reqwest::Url::parse(&self.api_url)
            .with_context(|| format!("invalid api_url {:?}", self.api_url))?;
        Ok(())
    }

    pub fn require_dest(&self) -> Result<&Path> {
        self.dest.as_deref().context(
            "no backup directory configured; pass --dest DIR or set `dest` in the config file \
             (run `code-backup init` to create one)",
        )
    }

    /// Directory for archives: configured, or `<dest>-archives` next to the backup directory.
    pub fn archive_dir(&self, dest: &Path) -> PathBuf {
        self.archive.dir.clone().unwrap_or_else(|| {
            let mut name = dest.file_name().unwrap_or_default().to_owned();
            name.push("-archives");
            dest.with_file_name(name)
        })
    }
}

pub const SAMPLE_CONFIG: &str = r#"# code-backup configuration
# Every setting can be overridden on the command line (see `code-backup sync --help`).

# Where repositories are stored. Layout: <dest>/<owner>/<repo>.git (mirror) or <dest>/<owner>/<repo> (checkout).
dest = "~/Backups/github"

# mirror   = bare repositories with every branch and tag (best for backups)
# checkout = normal clones with a working tree, fast-forwarded when there are no local changes
mode = "mirror"

# How git talks to GitHub:
#   ssh   = git@github.com:... using your SSH agent / ~/.ssh/config, or `ssh_key` below
#   https = https:// with a GitHub token (see token sources below)
#   gh    = https:// using the gh CLI login as the credential helper
transport = "ssh"
# ssh_key = "~/.ssh/id_ed25519"

# How repositories are discovered:
#   api = GitHub REST API with a token
#   gh  = `gh api` with your gh CLI login
discovery = "api"

# Token sources, in order: token_command, $GITHUB_TOKEN, $GH_TOKEN, `gh auth token`.
# token_command = "security find-generic-password -s github-backup -w"

# api_url = "https://api.github.com"   # GitHub Enterprise: "https://ghe.example.com/api/v3"

jobs = 8                 # parallel git operations
lfs = false              # also fetch Git LFS objects (needs git-lfs)
wikis = false            # also back up wikis as <repo>.wiki.git
pull_refs = false        # mirror mode: also keep refs/pull/* (pull request heads)
retries = 2              # retries for transient network failures
timeout = "60m"          # per git command

# What to do with local copies of repositories that disappeared from GitHub
# (deleted, or you lost access): keep | move | delete
orphans = "keep"

[select]
# mine = true                       # your repositories (default when no other source is listed)
affiliation = ["owner"]             # owner, collaborator, organization_member
# users = ["torvalds"]              # repositories owned by these users
# orgs = ["my-company"]             # repositories of these organizations
# repos = ["rust-lang/rust"]        # individual repositories (bypass the filters below)
# starred = false                   # repositories you starred

# Glob patterns. Without a '/' they match the repo name, with a '/' they match owner/name.
include = []
exclude = []
# exclude = ["*-old", "my-company/legacy-*"]

forks = true                        # include forks
archived = true                     # include archived repositories
visibility = "all"                  # all | public | private
# max_size_mb = 2000                # skip repositories larger than this (GitHub's size estimate)

[archive]
enabled = false                     # create an archive after every `sync`
# dir = "~/Backups/github-archives" # default: <dest>-archives
format = "tar.zst"                  # tar.zst | tar.gz | tar
level = 3                           # zstd 1-22, gzip 0-9
keep = 7                            # number of archives to keep (0 = keep all)
prefix = "github-backup"            # file name prefix
"#;

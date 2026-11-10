use crate::config::{ArchiveFormat, Discovery, Mode, Orphans, Transport, Visibility};
use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "code-backup",
    version,
    about = "Fast, incremental backups of your GitHub repositories.",
    after_help = "Get started: `code-backup init`, edit the config, then `code-backup sync`.\n\
                  Settings come from defaults, then the config file, then command-line flags."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug)]
pub struct GlobalArgs {
    /// Config file [default: ~/.config/code-backup/config.toml, if it exists]
    #[arg(short, long, global = true, env = "CODE_BACKUP_CONFIG", value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// More output: -v lists every repository, -vv also logs each git command
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Only print warnings, errors and the final summary
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Disable progress bars (they are also off when stderr is not a terminal)
    #[arg(long, global = true)]
    pub no_progress: bool,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once at startup
pub enum Command {
    /// Discover repositories, then clone new ones and update existing ones
    Sync(SyncArgs),
    /// Show which repositories the current selection matches
    List(ListArgs),
    /// Create a compressed archive (tar.zst, tar.gz or tar) of the backup directory
    Archive(ArchiveArgs),
    /// Check the integrity of the local backups with `git fsck`
    Verify(VerifyArgs),
    /// Check that git, gh, SSH and tokens are set up correctly
    Doctor(DoctorArgs),
    /// Write a commented starter config file
    Init(InitArgs),
}

/// Turn a `--foo` / `--no-foo` pair into an optional override.
pub fn flag(yes: bool, no: bool) -> Option<bool> {
    if yes {
        Some(true)
    } else if no {
        Some(false)
    } else {
        None
    }
}

#[derive(Args, Debug)]
pub struct DestArgs {
    /// Backup directory
    #[arg(short, long, env = "CODE_BACKUP_DEST", value_name = "DIR")]
    pub dest: Option<PathBuf>,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "GitHub access")]
pub struct ConnArgs {
    /// How to list repositories [default: api]
    #[arg(long, value_enum)]
    pub discovery: Option<Discovery>,

    /// GitHub API base URL (for GitHub Enterprise: https://HOST/api/v3)
    #[arg(long, value_name = "URL")]
    pub api_url: Option<String>,

    /// Shell command that prints a GitHub token (e.g. a password manager lookup)
    #[arg(long, value_name = "CMD")]
    pub token_command: Option<String>,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "Selection")]
pub struct SelectArgs {
    /// Include your own repositories (default unless other sources are given; disable with --no-mine)
    #[arg(long, overrides_with = "no_mine")]
    pub mine: bool,
    #[arg(long, hide = true, overrides_with = "mine")]
    pub no_mine: bool,

    /// Which of your repositories: owner, collaborator, organization_member [default: owner]
    #[arg(long, value_delimiter = ',', value_name = "LIST")]
    pub affiliation: Option<Vec<String>>,

    /// Back up repositories owned by this user (repeatable)
    #[arg(short, long, value_name = "USER")]
    pub user: Vec<String>,

    /// Back up repositories of this organization (repeatable)
    #[arg(short, long, value_name = "ORG")]
    pub org: Vec<String>,

    /// Back up this repository; bypasses include/exclude filters (repeatable)
    #[arg(short, long, value_name = "OWNER/NAME")]
    pub repo: Vec<String>,

    /// Include repositories you starred (disable with --no-starred)
    #[arg(long, overrides_with = "no_starred")]
    pub starred: bool,
    #[arg(long, hide = true, overrides_with = "starred")]
    pub no_starred: bool,

    /// Only keep repositories matching this glob; without '/' it matches the name, with '/' owner/name (repeatable)
    #[arg(short, long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Skip repositories matching this glob (repeatable)
    #[arg(short = 'x', long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Skip forks (include them again with --forks)
    #[arg(long, overrides_with = "forks")]
    pub no_forks: bool,
    #[arg(long, hide = true, overrides_with = "no_forks")]
    pub forks: bool,

    /// Skip archived repositories (include them again with --archived)
    #[arg(long, overrides_with = "archived")]
    pub no_archived: bool,
    #[arg(long, hide = true, overrides_with = "no_archived")]
    pub archived: bool,

    /// Only public or only private repositories [default: all]
    #[arg(long, value_enum)]
    pub visibility: Option<Visibility>,

    /// Skip repositories larger than this many MB (0 = no limit)
    #[arg(long, value_name = "MB")]
    pub max_size: Option<u64>,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "Sync")]
pub struct SyncOpts {
    /// Repository layout [default: mirror]
    #[arg(short, long, value_enum)]
    pub mode: Option<Mode>,

    /// How git authenticates [default: ssh]
    #[arg(short, long, value_enum)]
    pub transport: Option<Transport>,

    /// SSH private key to use with --transport ssh
    #[arg(long, value_name = "FILE")]
    pub ssh_key: Option<PathBuf>,

    /// Parallel git operations [default: 8]
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Fetch Git LFS objects (disable with --no-lfs)
    #[arg(long, overrides_with = "no_lfs")]
    pub lfs: bool,
    #[arg(long, hide = true, overrides_with = "lfs")]
    pub no_lfs: bool,

    /// Back up wikis too (disable with --no-wikis)
    #[arg(long, overrides_with = "no_wikis")]
    pub wikis: bool,
    #[arg(long, hide = true, overrides_with = "wikis")]
    pub no_wikis: bool,

    /// Mirror mode: also keep refs/pull/* (disable with --no-pull-refs)
    #[arg(long, overrides_with = "no_pull_refs")]
    pub pull_refs: bool,
    #[arg(long, hide = true, overrides_with = "pull_refs")]
    pub no_pull_refs: bool,

    /// What to do with local repositories that no longer exist on GitHub [default: keep]
    #[arg(long, value_enum)]
    pub orphans: Option<Orphans>,

    /// Retries for transient network failures [default: 2]
    #[arg(long, value_name = "N")]
    pub retries: Option<u32>,

    /// Time limit per git command, e.g. 30m or 2h [default: 60m]
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,

    /// Fetch every repository even if GitHub reports no new pushes, and lift the orphan safety limit
    #[arg(short, long)]
    pub force: bool,

    /// Show what would happen without changing anything
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Write a JSON report of the run to this file
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,

    /// Create an archive after syncing (disable with --no-archive)
    #[arg(long, overrides_with = "no_archive")]
    pub archive: bool,
    #[arg(long, hide = true, overrides_with = "archive")]
    pub no_archive: bool,
}

#[derive(Args, Debug)]
#[command(next_help_heading = "Archive")]
pub struct ArchiveOpts {
    /// Directory for archives [default: <dest>-archives]
    #[arg(long, value_name = "DIR")]
    pub archive_dir: Option<PathBuf>,

    /// Archive format [default: tar.zst]
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub archive_format: Option<ArchiveFormat>,

    /// Compression level: zstd 1-22, gzip 0-9 [default: 3]
    #[arg(long, value_name = "N", allow_negative_numbers = false)]
    pub archive_level: Option<i32>,

    /// Keep only the newest N archives (0 = keep all) [default: 0]
    #[arg(long, value_name = "N")]
    pub archive_keep: Option<usize>,

    /// Archive file name prefix [default: github-backup]
    #[arg(long, value_name = "NAME")]
    pub archive_prefix: Option<String>,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(flatten)]
    pub dest: DestArgs,
    #[command(flatten)]
    pub sync: SyncOpts,
    #[command(flatten)]
    pub select: SelectArgs,
    #[command(flatten)]
    pub conn: ConnArgs,
    #[command(flatten)]
    pub archive: ArchiveOpts,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,

    /// Also show repositories the filters excluded, with the reason
    #[arg(long)]
    pub show_excluded: bool,

    #[command(flatten)]
    pub select: SelectArgs,
    #[command(flatten)]
    pub conn: ConnArgs,
}

#[derive(Args, Debug)]
pub struct ArchiveArgs {
    #[command(flatten)]
    pub dest: DestArgs,
    #[command(flatten)]
    pub archive: ArchiveOpts,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    #[command(flatten)]
    pub dest: DestArgs,

    /// Full object check (slower); the default only checks connectivity
    #[arg(long)]
    pub full: bool,

    /// Parallel checks [default: number of CPUs]
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub dest: DestArgs,
    #[command(flatten)]
    pub conn: ConnArgs,

    /// Transport to check [default: from config, else ssh]
    #[arg(short, long, value_enum)]
    pub transport: Option<Transport>,

    /// SSH private key to test
    #[arg(long, value_name = "FILE")]
    pub ssh_key: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing config file
    #[arg(long)]
    pub force: bool,
}

# Code Backup

`code-backup` is a fast, incremental backup tool for GitHub repositories, written in Rust. Each run lists your repositories, clones new ones, and fetches only the ones that changed. It can also write a compressed archive of the whole collection.

- **Two layouts.** `mirror` keeps bare repositories with every branch and tag, which is the best format for backups. `checkout` keeps normal clones with working trees and fast-forwards them when that is safe.
- **Three ways to authenticate git.** `ssh` uses your agent, `~/.ssh/config`, or a specific key. `https` uses a GitHub token. `gh` reuses your `gh auth login` session.
- **Two ways to list repositories.** `api` calls the GitHub REST API directly. `gh` uses `gh api`.
- **Incremental runs.** Repositories whose GitHub `pushed_at` has not changed since the last successful sync are skipped without any network call. A second run over 180 repositories takes about 2 seconds, and most of that is the API listing.
- **Parallel jobs.** Git runs in parallel (8 jobs by default), and the largest repositories start first so the run finishes sooner.
- **Selection.** You can back up your own repositories, other users' repositories, organizations, starred repositories, and individual repositories. You can filter by include/exclude globs, forks, archived status, visibility, and size.
- **Archives.** Writes `tar.zst` (multithreaded), `tar.gz`, or `tar` snapshots, with a retention limit.
- **Safe for scheduled runs.** A lock file prevents overlapping runs. New clones are built in `*.partial` directories and only moved into place when complete. The state file is written atomically. Ctrl-C or SIGTERM stops the git processes cleanly. Exit codes are meaningful for cron and launchd.
- **Renames and transfers.** These are detected by repository id, and the local copy is moved instead of downloaded again.
- **Orphans.** Repositories deleted from GitHub (or that you lost access to) are reported. Depending on settings, they are kept, moved aside, or deleted, and a safety limit stops mass deletion.
- **Extras.** Optional wikis, Git LFS objects, and `refs/pull/*`. `verify` runs `git fsck` over everything. `doctor` checks your setup.

## Install

You need Rust 1.89 or newer, and git 2.31 or newer. `gh` and `git-lfs` are optional.

```sh
cargo install --path .
# or: cargo build --release && cp target/release/code-backup ~/.local/bin/
```

## Quick start

```sh
code-backup init                 # writes ~/.config/code-backup/config.toml
$EDITOR ~/.config/code-backup/config.toml
code-backup doctor               # checks git, gh, token, SSH and the backup directory
code-backup list                 # shows what would be backed up
code-backup sync --dry-run       # shows what sync would do
code-backup sync                 # does it
```

You can do everything from the command line without a config file:

```sh
code-backup sync -d ~/Backups/github
code-backup sync -d ~/Backups/github -t gh --discovery gh       # only the gh CLI, no token handling
code-backup sync -d ~/Backups/github -m checkout -x '*-old' --no-forks
code-backup sync -d ~/Backups/github -o my-company --affiliation owner,organization_member
code-backup sync -d ~/Backups/github --archive --archive-keep 14
```

## Commands

| Command   | What it does |
|-----------|--------------|
| `sync`    | Lists repositories, clones new ones, and updates changed ones. `--archive` adds a snapshot at the end. |
| `list`    | Prints the selected repositories (`--json`, `--show-excluded` shows why each repository was excluded). |
| `archive` | Writes `<prefix>-YYYYmmdd-HHMMSS.tar.zst` (or `.tar.gz` / `.tar`) of the backup directory. |
| `verify`  | Runs `git fsck` on every backed-up repository (`--full` checks every object). |
| `doctor`  | Checks the git version, git-lfs, the gh login, the token and its scopes, SSH authentication, and the backup directory. |
| `init`    | Writes a commented starter config. |

Run `code-backup <command> --help` to see every option.

## How it works

### Layout

```
~/Backups/github/
├── .code-backup/state.json     # what was synced, when, and GitHub's pushed_at at that time
├── .code-backup/lock           # held while a run is active
├── .orphaned/<timestamp>/...   # only with orphans = "move"
├── robert-mcdermott/
│   ├── godu.git                # mirror mode
│   ├── godu.wiki.git           # with --wikis
│   └── ...
└── my-company/
    └── service                 # checkout mode
```

### Modes

- **mirror** (the default) creates a bare repository with the refspecs `refs/heads/*`, `refs/tags/*` and `refs/notes/*`. Add `--pull-refs` to also keep `refs/pull/*`. Updates run `git fetch --prune`, so the backup matches GitHub exactly. HEAD follows the repository's default branch.
- **checkout** creates a normal clone. Updates fetch all branches. They fast-forward the current branch only if the working tree is clean and has no local commits. Otherwise you get a warning and the local work is left alone.

Switching mode later creates the new layout next to the old one. The old directories are kept and are no longer updated.

### Transports

| Transport | URL | Authentication |
|-----------|-----|----------------|
| `ssh` (default) | `git@github.com:owner/repo.git` | Your SSH agent and `~/.ssh/config`, or `--ssh-key FILE`. Runs with `BatchMode=yes`, so it never prompts. |
| `https` | `https://github.com/owner/repo.git` | A token, passed to git through `GIT_CONFIG_*` environment variables. |
| `gh` | `https://github.com/owner/repo.git` | `gh auth git-credential`, so it works anywhere `gh auth login` was done. |

Tokens never appear in command-line arguments (which `ps` can show), in remote URLs, or in `.git/config`. You can switch transport at any time, and existing backups get their remote URL updated on the next fetch.

### Tokens

The `api` discovery and the `https` transport need a token. `code-backup` looks in these places, in order:

1. `token_command` in the config (for example `security find-generic-password -s github-backup -w`, or `op read op://...`)
2. `$GITHUB_TOKEN`
3. `$GH_TOKEN`
4. `gh auth token`

A classic token needs the `repo` scope to see private repositories. A fine-grained token needs read access to Contents and Metadata.

### Incremental sync

When a fetch succeeds, the repository's `pushed_at` is stored. On the next run, a repository with the same `pushed_at` is skipped without contacting GitHub. Use `--force` to fetch everything anyway, for example as a weekly safety net. A repository whose last sync failed, or whose relevant settings changed (mode, LFS, pull refs), is always fetched. Wikis are always fetched, because `pushed_at` does not track wiki edits. A repository that has no wiki is checked again only when the repository itself changes.

### Selection and filters

- Sources: your repositories (`--mine`, which is the default when no other source is given), `--user`, `--org`, `--starred`, and `--repo OWNER/NAME`.
- `--affiliation` controls which of *your* repositories are included: `owner` (the default), `collaborator`, and `organization_member`.
- `--include` and `--exclude` take globs that are matched case-insensitively. A pattern without `/` matches the repository name (`-x '*-archive'`). A pattern with `/` matches `owner/name` (`-i 'my-org/*'`).
- `--no-forks`, `--no-archived`, `--visibility public|private`, and `--max-size MB` (GitHub's size estimate).
- Repositories named with `--repo` skip the include and exclude filters.

Command-line values replace the matching config value; for example, `-x foo` replaces the config's `exclude` list.

### Orphans

A backed-up repository is an *orphan* when no configured source returns it any more, because it was deleted or you lost access. A repository that is only filtered out is not an orphan, and its backup is left untouched. Orphans are handled according to `--orphans`:

- `keep` (the default) lists them in the summary.
- `move` moves them to `<dest>/.orphaned/<timestamp>/`.
- `delete` removes them.

Orphan handling is skipped when any source failed to list. It is also skipped when more than 20% of the backup (and more than 5 repositories) would be affected, unless you pass `--force`.

### Archives

`sync --archive` (or `archive.enabled = true`) and `code-backup archive` write a single tarball to `<dest>-archives/`, or to the directory set with `--archive-dir`. The tarball skips lock files and unfinished clones. zstd uses every CPU core. `--archive-keep N` deletes all but the newest N archives that have the same prefix and format.

Git pack files are already compressed, so `tar.zst` at level 3 is fast and almost as small as higher levels. Archives also give you point-in-time history: a mirror follows force-pushes and branch deletions on GitHub, but an old archive still has the earlier state.

### Restoring

```sh
git clone ~/Backups/github/owner/repo.git repo         # from a mirror
# or push a mirror to a new remote:
git -C ~/Backups/github/owner/repo.git push --mirror git@github.com:owner/repo.git
# from an archive:
zstd -dc github-backup-20261109-020000.tar.zst | tar -xf -
```

## Scheduling

### macOS (launchd)

Save this as `~/Library/LaunchAgents/com.example.code-backup.plist`, and adjust the paths:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.example.code-backup</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/YOU/.cargo/bin/code-backup</string>
    <string>sync</string>
    <string>--quiet</string>
    <string>--report</string><string>/Users/YOU/Backups/last-run.json</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string></dict>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>2</integer><key>Minute</key><integer>30</integer></dict>
  <key>StandardOutPath</key><string>/Users/YOU/Backups/code-backup.log</string>
  <key>StandardErrorPath</key><string>/Users/YOU/Backups/code-backup.log</string>
</dict>
</plist>
```

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.example.code-backup.plist
launchctl kickstart gui/$(id -u)/com.example.code-backup     # run it now to test
```

For unattended runs, `transport = "gh"` (or `https` with a token from the Keychain through `token_command`) avoids needing an unlocked SSH key. `PATH` must include the directory that contains `gh`.

### Linux / cron

```cron
30 2 * * *  PATH=/usr/local/bin:/usr/bin:/bin  $HOME/.cargo/bin/code-backup sync -q >> $HOME/code-backup.log 2>&1
0 4 * * 0   PATH=/usr/local/bin:/usr/bin:/bin  $HOME/.cargo/bin/code-backup sync -q --force --archive >> $HOME/code-backup.log 2>&1
```

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Everything succeeded |
| 1    | Some repositories failed, a source could not be listed, or the archive failed |
| 2    | Configuration or usage error, or another run holds the lock |
| 130  | Interrupted (Ctrl-C / SIGTERM); finished work is saved |

## Output and logging

- When stderr is a terminal, the tool shows progress bars with one spinner per running job. When it is not (cron, launchd, pipes), it prints plain lines.
- `-v` lists every repository, including unchanged ones. `-vv` also lists excluded repositories and logs every git command. `-q` prints only warnings, errors, and the summary.
- `--report FILE` writes a JSON report with per-repository status, timings, and errors.
- `CODE_BACKUP_LOG=debug` sets the log filter directly.

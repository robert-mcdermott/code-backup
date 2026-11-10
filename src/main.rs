mod archive;
mod auth;
mod cli;
mod config;
mod discover;
mod doctor;
mod filter;
mod git;
mod list;
mod ops;
mod state;
mod sync;
mod ui;
mod util;
mod verify;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Cli, Command};
use config::{SAMPLE_CONFIG, Settings};
use std::process::ExitCode;
use ui::Ui;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.global.verbose, cli.global.quiet);
    let ui = Ui::new(&cli.global);
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            ui.error(format!("starting async runtime: {e}"));
            return ExitCode::from(2);
        }
    };
    match runtime.block_on(run(cli, ui.clone())) {
        Ok(code) => code,
        Err(e) => {
            ui.error(format!("{e:#}"));
            ExitCode::from(2)
        }
    }
}

fn init_tracing(verbose: u8, quiet: bool) {
    let default = match (quiet, verbose) {
        (true, _) => "error",
        (_, 0 | 1) => "warn",
        (_, 2) => "code_backup=debug",
        _ => "debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("CODE_BACKUP_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();
}

async fn run(cli: Cli, ui: Ui) -> Result<ExitCode> {
    if let Command::Init(args) = &cli.command {
        return init(cli.global.config.as_deref(), args.force, &ui);
    }

    let (file, path) = config::load_file(cli.global.config.as_deref())?;
    let mut settings = Settings::from_file(file, path)?;

    match cli.command {
        Command::Sync(a) => {
            settings.apply_dest(&a.dest);
            settings.apply_conn(&a.conn);
            settings.apply_select(&a.select);
            settings.apply_sync(&a.sync)?;
            settings.apply_archive(&a.archive);
            settings.validate()?;
            sync::run(settings, ui).await
        }
        Command::List(a) => {
            settings.apply_conn(&a.conn);
            settings.apply_select(&a.select);
            settings.validate()?;
            list::run(&settings, &ui, a.json, a.show_excluded).await
        }
        Command::Archive(a) => {
            settings.apply_dest(&a.dest);
            settings.apply_archive(&a.archive);
            settings.validate()?;
            let ui2 = ui.clone();
            tokio::task::spawn_blocking(move || archive::run(&settings, &ui2))
                .await
                .context("archive task panicked")?
        }
        Command::Verify(a) => {
            settings.apply_dest(&a.dest);
            settings.validate()?;
            verify::run(&settings, &ui, a.full, a.jobs).await
        }
        Command::Doctor(a) => {
            settings.apply_dest(&a.dest);
            settings.apply_conn(&a.conn);
            if let Some(t) = a.transport {
                settings.transport = t;
            }
            if let Some(k) = a.ssh_key {
                settings.ssh_key = Some(k);
            }
            doctor::run(&settings, &ui).await
        }
        Command::Init(_) => unreachable!("handled above"),
    }
}

fn init(explicit: Option<&std::path::Path>, force: bool, ui: &Ui) -> Result<ExitCode> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => config::default_config_path().context("cannot determine the config directory; pass --config")?,
    };
    if path.exists() && !force {
        bail!("{} already exists (use --force to overwrite)", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, SAMPLE_CONFIG).with_context(|| format!("writing {}", path.display()))?;
    ui.always(format!("Wrote {}", path.display()));
    ui.always("Edit it, then check your setup with `code-backup doctor` and run `code-backup sync`.");
    Ok(ExitCode::SUCCESS)
}

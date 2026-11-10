use crate::config::Settings;
use crate::discover::{Repo, discover};
use crate::filter::Filter;
use crate::sync::connect;
use crate::ui::Ui;
use anyhow::Result;
use console::style;
use serde::Serialize;
use std::process::ExitCode;

#[derive(Serialize)]
struct Row<'a> {
    full_name: &'a str,
    private: bool,
    fork: bool,
    archived: bool,
    size_kb: u64,
    pushed_at: Option<&'a str>,
    default_branch: Option<&'a str>,
    has_wiki: bool,
    ssh_url: &'a str,
    clone_url: &'a str,
    selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    excluded_because: Option<String>,
}

fn human_kb(kb: u64) -> String {
    match kb {
        0..1024 => format!("{kb} KB"),
        1024..1_048_576 => format!("{:.1} MB", kb as f64 / 1024.0),
        _ => format!("{:.1} GB", kb as f64 / 1_048_576.0),
    }
}

pub async fn run(settings: &Settings, ui: &Ui, json: bool, show_excluded: bool) -> Result<ExitCode> {
    let access = connect(settings, false).await?;
    let disc = discover(&access.backend, &settings.select, access.authenticated).await;
    for e in &disc.errors {
        ui.error(e);
    }
    let filter = Filter::new(&settings.select)?;
    let checked: Vec<(&Repo, Result<(), String>)> =
        disc.repos.iter().map(|r| (r, filter.check(r))).collect();
    let selected = checked.iter().filter(|(_, c)| c.is_ok()).count();
    let excluded = checked.len() - selected;
    let shown: Vec<&(&Repo, Result<(), String>)> =
        checked.iter().filter(|(_, c)| show_excluded || c.is_ok()).collect();

    if json {
        let rows: Vec<Row> = shown
            .iter()
            .map(|(r, c)| Row {
                full_name: &r.full_name,
                private: r.private,
                fork: r.fork,
                archived: r.archived,
                size_kb: r.size,
                pushed_at: r.pushed_at.as_deref(),
                default_branch: r.default_branch.as_deref(),
                has_wiki: r.has_wiki,
                ssh_url: &r.ssh_url,
                clone_url: &r.clone_url,
                selected: c.is_ok(),
                excluded_because: c.clone().err(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        let width = shown.iter().map(|(r, _)| r.full_name.len()).max().unwrap_or(4).max(4);
        println!(
            "{}",
            style(format!(
                "{:<width$}  {:<7}  {:>9}  {:<10}  {}",
                "REPOSITORY", "VISIB.", "SIZE", "PUSHED", "NOTES"
            ))
            .bold()
        );
        for (r, c) in &shown {
            let mut notes = Vec::new();
            if r.fork {
                notes.push("fork".to_owned());
            }
            if r.archived {
                notes.push("archived".to_owned());
            }
            if let Err(reason) = c {
                notes.push(format!("excluded: {reason}"));
            }
            let line = format!(
                "{:<width$}  {:<7}  {:>9}  {:<10}  {}",
                r.full_name,
                if r.private { "private" } else { "public" },
                human_kb(r.size),
                r.pushed_at.as_deref().map(|p| &p[..p.len().min(10)]).unwrap_or("-"),
                notes.join(", ")
            );
            if c.is_ok() {
                println!("{line}");
            } else {
                println!("{}", style(line).dim());
            }
        }
    }
    let total_kb: u64 = checked.iter().filter(|(_, c)| c.is_ok()).map(|(r, _)| r.size).sum();
    eprintln!(
        "{} selected ({}), {excluded} excluded",
        crate::util::plural(selected, "repository", "repositories"),
        human_kb(total_kb)
    );
    Ok(if disc.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

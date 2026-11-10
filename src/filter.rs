use crate::config::{Selection, Visibility};
use crate::discover::Repo;
use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::collections::HashSet;

/// Glob patterns: without '/' they match the repository name, with '/' they
/// match `owner/name`. Matching is case-insensitive like GitHub names.
struct Patterns {
    names: GlobSet,
    name_src: Vec<String>,
    full: GlobSet,
    full_src: Vec<String>,
}

impl Patterns {
    fn new(patterns: &[String]) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut full = GlobSetBuilder::new();
        let mut name_src = Vec::new();
        let mut full_src = Vec::new();
        for p in patterns {
            let glob = GlobBuilder::new(p)
                .case_insensitive(true)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid pattern {p:?}"))?;
            if p.contains('/') {
                full.add(glob);
                full_src.push(p.clone());
            } else {
                names.add(glob);
                name_src.push(p.clone());
            }
        }
        Ok(Patterns {
            names: names.build()?,
            name_src,
            full: full.build()?,
            full_src,
        })
    }

    fn is_empty(&self) -> bool {
        self.name_src.is_empty() && self.full_src.is_empty()
    }

    /// The first pattern that matches, if any.
    fn find(&self, repo: &Repo) -> Option<&str> {
        if let Some(i) = self.names.matches(&repo.name).first() {
            return Some(&self.name_src[*i]);
        }
        if let Some(i) = self.full.matches(&repo.full_name).first() {
            return Some(&self.full_src[*i]);
        }
        None
    }
}

pub struct Filter {
    include: Patterns,
    exclude: Patterns,
    explicit: HashSet<String>,
    forks: bool,
    archived: bool,
    visibility: Visibility,
    max_size_kb: Option<u64>,
}

impl Filter {
    pub fn new(sel: &Selection) -> Result<Self> {
        Ok(Filter {
            include: Patterns::new(&sel.include)?,
            exclude: Patterns::new(&sel.exclude)?,
            explicit: sel.repos.iter().map(|r| r.to_ascii_lowercase()).collect(),
            forks: sel.forks,
            archived: sel.archived,
            visibility: sel.visibility,
            max_size_kb: sel.max_size_mb.map(|mb| mb * 1024),
        })
    }

    /// `Ok` if the repository should be backed up, otherwise the reason it is skipped.
    pub fn check(&self, repo: &Repo) -> Result<(), String> {
        if repo.disabled {
            return Err("disabled by GitHub".into());
        }
        if self.explicit.contains(&repo.full_name.to_ascii_lowercase()) {
            return Ok(());
        }
        if !self.include.is_empty() && self.include.find(repo).is_none() {
            return Err("no include pattern matches".into());
        }
        if let Some(p) = self.exclude.find(repo) {
            return Err(format!("excluded by {p:?}"));
        }
        if !self.forks && repo.fork {
            return Err("fork".into());
        }
        if !self.archived && repo.archived {
            return Err("archived".into());
        }
        match self.visibility {
            Visibility::Public if repo.private => return Err("private".into()),
            Visibility::Private if !repo.private => return Err("public".into()),
            _ => {}
        }
        if let Some(max) = self.max_size_kb
            && repo.size > max {
                return Err(format!("larger than {} MB", max / 1024));
            }
        Ok(())
    }
}

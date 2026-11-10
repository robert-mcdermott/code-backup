use crate::config::{ArchiveFormat, Settings};
use crate::state::{META_DIR, Store};
use crate::ui::Ui;
use anyhow::{Context, Result, bail};
use indicatif::{HumanBytes, ProgressBar};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// `code-backup archive`: lock the backup directory and archive it.
pub fn run(settings: &Settings, ui: &Ui) -> Result<ExitCode> {
    let root = settings.require_dest()?;
    if !root.is_dir() {
        bail!("backup directory {} does not exist; run `code-backup sync` first", root.display());
    }
    let _store = Store::open(root)?;
    create(root, settings, ui)?;
    Ok(ExitCode::SUCCESS)
}

enum ItemKind {
    Dir,
    File(u64),
    Symlink,
}

struct Item {
    abs: PathBuf,
    rel: PathBuf,
    kind: ItemKind,
}

/// Write `<prefix>-<YYYYmmdd-HHMMSS>.<ext>` containing the backup directory,
/// then apply the retention policy. The caller must hold the directory lock.
pub fn create(root: &Path, settings: &Settings, ui: &Ui) -> Result<PathBuf> {
    let a = &settings.archive;
    let out_dir = settings.archive_dir(root);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let root_c = root.canonicalize()?;
    let out_c = out_dir.canonicalize()?;
    if out_c.starts_with(&root_c) {
        bail!(
            "archive directory {} is inside the backup directory {}",
            out_dir.display(),
            root.display()
        );
    }

    let stem = format!("{}-{}", a.prefix, chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let file_name = format!("{stem}.{}", a.format.extension());
    let final_path = out_dir.join(&file_name);
    if final_path.exists() {
        bail!("{} already exists", final_path.display());
    }
    let tmp = out_dir.join(format!("{file_name}.partial"));

    let items = collect(root)?;
    let total: u64 = items
        .iter()
        .map(|i| match i.kind {
            ItemKind::File(n) => n,
            _ => 0,
        })
        .sum();
    ui.info(format!(
        "Archiving {} ({} entries, {}) to {}",
        root.display(),
        items.len(),
        HumanBytes(total),
        final_path.display()
    ));
    let pb = ui.byte_bar(total, &file_name);

    let result = write_archive(&tmp, a.format, a.level, &items, &stem, &pb);
    ui.remove(&pb);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &final_path).with_context(|| format!("renaming to {}", final_path.display()))?;
    let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    ui.always(format!(
        "{} {} ({})",
        console::style("Archive written:").bold(),
        final_path.display(),
        HumanBytes(size)
    ));

    prune(&out_dir, &a.prefix, a.format, a.keep, ui)?;
    Ok(final_path)
}

fn write_archive(
    tmp: &Path,
    format: ArchiveFormat,
    level: i32,
    items: &[Item],
    stem: &str,
    pb: &ProgressBar,
) -> Result<()> {
    let file = File::create(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let w = BufWriter::with_capacity(1 << 20, file);
    let w = match format {
        ArchiveFormat::TarZst => {
            let mut enc = zstd::stream::write::Encoder::new(w, level)?;
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
            enc.multithread(threads)?;
            enc.include_checksum(true)?;
            write_tar(enc, items, stem, pb)?.finish()?
        }
        ArchiveFormat::TarGz => {
            let enc = flate2::write::GzEncoder::new(w, flate2::Compression::new(level as u32));
            write_tar(enc, items, stem, pb)?.finish()?
        }
        ArchiveFormat::Tar => write_tar(w, items, stem, pb)?,
    };
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
}

fn write_tar<W: Write>(w: W, items: &[Item], stem: &str, pb: &ProgressBar) -> Result<W> {
    let mut b = tar::Builder::new(w);
    b.mode(tar::HeaderMode::Complete);
    b.follow_symlinks(false);
    for item in items {
        let name = Path::new(stem).join(&item.rel);
        match item.kind {
            ItemKind::Dir => b.append_dir(&name, &item.abs)?,
            ItemKind::File(size) => {
                let f = File::open(&item.abs).with_context(|| format!("opening {}", item.abs.display()))?;
                let meta = f.metadata()?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
                header.set_size(size);
                let reader = ExactReader {
                    inner: f,
                    remaining: size,
                    pb,
                };
                b.append_data(&mut header, &name, reader)
                    .with_context(|| format!("adding {}", item.abs.display()))?;
            }
            ItemKind::Symlink => {
                let meta = std::fs::symlink_metadata(&item.abs)?;
                let target = std::fs::read_link(&item.abs)?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                b.append_link(&mut header, &name, &target)?;
            }
        }
    }
    Ok(b.into_inner()?)
}

/// Reads exactly `remaining` bytes and reports progress. Fails if the file
/// shrank, which would otherwise produce a corrupt tar entry.
struct ExactReader<'a> {
    inner: File,
    remaining: u64,
    pb: &'a ProgressBar,
}

impl Read for ExactReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..cap])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file changed while it was being archived",
            ));
        }
        self.remaining -= n as u64;
        self.pb.inc(n as u64);
        Ok(n)
    }
}

/// Everything under the backup directory except lock files and unfinished clones.
fn collect(root: &Path) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            let in_meta = e.depth() == 2
                && e.path().parent().and_then(|p| p.file_name()).is_some_and(|n| n == META_DIR);
            !(name.ends_with(".partial") || (in_meta && (name == "lock" || name.ends_with(".tmp"))))
        });
    for entry in walker {
        let entry = entry?;
        let rel = entry.path().strip_prefix(root)?.to_path_buf();
        let ft = entry.file_type();
        let kind = if ft.is_dir() {
            ItemKind::Dir
        } else if ft.is_file() {
            ItemKind::File(entry.metadata()?.len())
        } else if ft.is_symlink() {
            ItemKind::Symlink
        } else {
            continue; // sockets, fifos
        };
        items.push(Item {
            abs: entry.path().to_path_buf(),
            rel,
            kind,
        });
    }
    Ok(items)
}

/// Keep the newest `keep` archives with this prefix and format.
fn prune(dir: &Path, prefix: &str, format: ArchiveFormat, keep: usize, ui: &Ui) -> Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let head = format!("{prefix}-");
    let tail = format!(".{}", format.extension());
    let mut archives: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix(&head)
                .and_then(|r| r.strip_suffix(&tail))
                .is_some_and(is_stamp)
        })
        .map(|e| e.path())
        .collect();
    archives.sort();
    while archives.len() > keep {
        let old = archives.remove(0);
        std::fs::remove_file(&old).with_context(|| format!("removing {}", old.display()))?;
        ui.info(format!("Removed old archive {}", old.display()));
    }
    Ok(())
}

/// `YYYYmmdd-HHMMSS`
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15
        && b[8] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 8 || c.is_ascii_digit())
}

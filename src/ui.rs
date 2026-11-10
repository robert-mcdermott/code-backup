use crate::cli::GlobalArgs;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

/// Terminal output: progress bars when interactive, plain lines otherwise
/// (cron, launchd, pipes).
#[derive(Clone)]
pub struct Ui {
    inner: Arc<Inner>,
}

struct Inner {
    mp: Option<MultiProgress>,
    verbose: u8,
    quiet: bool,
}

impl Ui {
    pub fn new(g: &GlobalArgs) -> Self {
        let interactive = std::io::stderr().is_terminal();
        let progress = interactive && !g.quiet && !g.no_progress && g.verbose < 2;
        let mp = progress.then(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr()));
        Ui {
            inner: Arc::new(Inner {
                mp,
                verbose: g.verbose,
                quiet: g.quiet,
            }),
        }
    }

    fn print(&self, msg: &str, to_stderr: bool) {
        match &self.inner.mp {
            Some(mp) if !mp.is_hidden() => {
                let _ = mp.println(msg);
            }
            _ if to_stderr => eprintln!("{msg}"),
            _ => println!("{msg}"),
        }
    }

    /// Normal progress information (hidden by --quiet).
    pub fn info(&self, msg: impl AsRef<str>) {
        if !self.inner.quiet {
            self.print(msg.as_ref(), false);
        }
    }

    /// Per-item details only shown with -v.
    pub fn detail(&self, msg: impl AsRef<str>) {
        if self.inner.verbose >= 1 && !self.inner.quiet {
            self.print(msg.as_ref(), false);
        }
    }

    /// Very detailed output, only shown with -vv.
    pub fn debug(&self, msg: impl AsRef<str>) {
        if self.inner.verbose >= 2 && !self.inner.quiet {
            self.print(msg.as_ref(), false);
        }
    }

    /// Always shown (summaries).
    pub fn always(&self, msg: impl AsRef<str>) {
        self.print(msg.as_ref(), false);
    }

    pub fn warn(&self, msg: impl AsRef<str>) {
        let line = format!("{} {}", console::style("warning:").yellow().bold(), msg.as_ref());
        self.print(&line, true);
    }

    pub fn error(&self, msg: impl AsRef<str>) {
        let line = format!("{} {}", console::style("error:").red().bold(), msg.as_ref());
        self.print(&line, true);
    }

    /// Overall progress bar for `len` items.
    pub fn bar(&self, len: u64, msg: &str) -> ProgressBar {
        let Some(mp) = &self.inner.mp else {
            return ProgressBar::hidden();
        };
        let pb = mp.add(ProgressBar::new(len));
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:32.cyan/blue}] {pos}/{len} {msg}",
            )
            .expect("valid template")
            .progress_chars("=> "),
        );
        pb.set_message(msg.to_owned());
        pb.enable_steady_tick(Duration::from_millis(120));
        pb
    }

    /// Byte-based progress bar (archives).
    pub fn byte_bar(&self, total: u64, msg: &str) -> ProgressBar {
        let Some(mp) = &self.inner.mp else {
            return ProgressBar::hidden();
        };
        let pb = mp.add(ProgressBar::new(total));
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:32.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} {msg}",
            )
            .expect("valid template")
            .progress_chars("=> "),
        );
        pb.set_message(msg.to_owned());
        pb.enable_steady_tick(Duration::from_millis(120));
        pb
    }

    /// Spinner for one in-flight job, shown above the main bar.
    pub fn spinner(&self, above: &ProgressBar, label: &str) -> ProgressBar {
        let Some(mp) = &self.inner.mp else {
            return ProgressBar::hidden();
        };
        let pb = mp.insert_before(above, ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("  {spinner:.dim} {prefix:.bold} {wide_msg:.dim} {elapsed:>4.dim}")
                .expect("valid template"),
        );
        pb.set_prefix(label.to_owned());
        pb.enable_steady_tick(Duration::from_millis(120));
        pb
    }

    pub fn remove(&self, pb: &ProgressBar) {
        pb.finish_and_clear();
        if let Some(mp) = &self.inner.mp {
            mp.remove(pb);
        }
    }
}

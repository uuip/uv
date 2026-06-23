use std::env;
use std::fmt::Write;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use owo_colors::OwoColorize;
use rustc_hash::FxHashMap;
use uv_static::EnvVars;

use crate::commands::human_readable_bytes;
use crate::printer::Printer;

/// Since downloads run in parallel, their message output order is non-deterministic, so
/// can't capture them in test output.
static HAS_UV_TEST_NO_CLI_PROGRESS: LazyLock<bool> =
    LazyLock::new(|| env::var(EnvVars::UV_TEST_NO_CLI_PROGRESS).is_ok());

/// Format bytes as MiB for progress templates.
///
/// Done via integer arithmetic so the byte counters never lose precision and clippy stays
/// quiet about `u64 as f64`.
fn write_mib(w: &mut dyn Write, bytes: u64) -> std::fmt::Result {
    const MIB: u64 = 1024 * 1024;
    let whole = bytes / MIB;
    let hundredths = (bytes % MIB) * 100 / MIB;
    write!(w, "{whole:>2}.{hundredths:02} MiB")
}

/// Progress reporter for `uv download`, which streams wheel/sdist artifacts directly to
/// the output directory without going through the preparer.
///
/// Per-artifact byte-level sub-bars are laid out on two lines (filename on one, bar +
/// bytes on the next). Wheel filenames with manylinux tags routinely exceed 90 characters,
/// which overflows any single-line template the moment a bar is appended, so the two-line
/// layout is what keeps the display readable regardless of terminal width.
#[derive(Debug)]
pub(crate) struct DownloadProjectReporter {
    printer: Printer,
    /// `None` in environments that do not render concurrent progress bars well (e.g. Jupyter).
    multi: Option<DownloadMulti>,
}

#[derive(Debug)]
struct DownloadMulti {
    multi_progress: MultiProgress,
    state: Arc<Mutex<DownloadBarState>>,
}

#[derive(Debug, Default)]
struct DownloadBarState {
    bars: FxHashMap<usize, ProgressBar>,
    next_id: usize,
    starting: Option<ProgressBar>,
}

impl DownloadProjectReporter {
    pub(crate) fn new(printer: Printer) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());

        // Jupyter cannot redraw previous lines, so multi-bar rendering is disabled there.
        // See https://github.com/astral-sh/uv/issues/3887.
        let multi = if env::var(EnvVars::JPY_SESSION_NAME).is_ok() {
            None
        } else {
            Some(DownloadMulti {
                multi_progress,
                state: Arc::default(),
            })
        };

        Self { printer, multi }
    }

    /// Register a single artifact as soon as its download task begins.
    pub(crate) fn on_download_start(&self) -> usize {
        let Some(multi) = self.multi.as_ref() else {
            return 0;
        };

        let mut state = multi.state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;

        if state.starting.is_none() {
            if multi.multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
                let _ = writeln!(self.printer.stderr(), "Starting downloads...");
            }

            let progress = multi
                .multi_progress
                .add(ProgressBar::with_draw_target(None, self.printer.target()));
            progress.set_style(
                ProgressStyle::with_template("{spinner:.green} {wide_msg:.dim}")
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            progress.enable_steady_tick(Duration::from_millis(100));
            progress.set_message("Starting downloads...");
            state.starting = Some(progress);
        }
        id
    }

    /// Open a byte-level progress bar once the server responds. If the server omits
    /// `Content-Length`, use a spinner for that artifact.
    pub(crate) fn on_download_response(&self, id: usize, name: String, size: Option<u64>) {
        let Some(multi) = self.multi.as_ref() else {
            return;
        };

        if let Some(progress) = multi.state.lock().unwrap().starting.take() {
            progress.finish_and_clear();
        }

        let progress = multi
            .multi_progress
            .add(ProgressBar::with_draw_target(size, self.printer.target()));

        if let Some(size) = size {
            // Two-line template:
            //   line 1: full filename (truncated by `wide_msg` if wider than the terminal).
            //   line 2: bar + binary bytes counters.
            // Keeping filename and bar on separate lines avoids the wrap corruption that
            // happened when a long wheel filename + 30-char bar exceeded the terminal width.
            // `{wide_bar}` fills the remaining terminal width instead of a fixed 40 chars,
            // so narrow terminals (<60 col) don't re-wrap the bar onto a third line.
            progress.set_style(
                ProgressStyle::with_template(
                    "{wide_msg:.cyan}\n  {wide_bar:.green/dim} {mib_pos}/{mib_len}  ",
                )
                .unwrap()
                .with_key("mib_pos", |state: &ProgressState, w: &mut dyn Write| {
                    let _ = write_mib(w, state.pos());
                })
                .with_key("mib_len", |state: &ProgressState, w: &mut dyn Write| {
                    let _ = write_mib(w, state.len().unwrap_or(0));
                })
                .progress_chars("--"),
            );
            if multi.multi_progress.is_hidden()
                && !*HAS_UV_TEST_NO_CLI_PROGRESS
                && size > 1024 * 1024
            {
                let _ = writeln!(
                    self.printer.stderr(),
                    "{} {} {}",
                    "Downloading".bold().cyan(),
                    name,
                    format!("({:.1})", human_readable_bytes(size)).dimmed()
                );
            }
            progress.set_message(name);
        } else {
            // Unknown content-length: use an animated spinner so the user can tell the
            // download is still in flight. Previously we called `progress.finish()` here,
            // which left a static "... ...." line that looked frozen.
            progress.set_style(
                ProgressStyle::with_template("  {spinner:.green} {wide_msg:.dim}")
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            progress.enable_steady_tick(Duration::from_millis(100));
            if multi.multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
                let _ = writeln!(
                    self.printer.stderr(),
                    "{} {}",
                    "Downloading".bold().cyan(),
                    name
                );
            }
            progress.set_message(name);
        }

        multi.state.lock().unwrap().bars.insert(id, progress);
    }

    pub(crate) fn on_download_progress(&self, id: usize, bytes: u64) {
        let Some(multi) = self.multi.as_ref() else {
            return;
        };
        if let Some(bar) = multi.state.lock().unwrap().bars.get(&id) {
            bar.inc(bytes);
        }
    }

    /// Close the per-artifact byte-level bar.
    ///
    /// Only paired with [`Self::on_download_start`] on the write path; files that are
    /// already materialized don't open a byte bar, so they must not call this.
    pub(crate) fn on_download_complete(&self, id: usize) {
        let Some(multi) = self.multi.as_ref() else {
            return;
        };
        let bar = multi.state.lock().unwrap().bars.remove(&id);
        if let Some(bar) = bar {
            bar.finish_and_clear();
        }
    }

    /// Drain any sub-bars still registered when the run finishes.
    ///
    /// Matters when the outer download loop aborts in-flight tasks after a failure: those
    /// futures are dropped at their `.await` point so `on_download_complete` never runs
    /// for them, leaving their bars drawn on the terminal until something clears them.
    /// Draining here guarantees the terminal is clean regardless of success or early abort.
    pub(crate) fn on_complete(&self) {
        if let Some(multi) = self.multi.as_ref() {
            let (starting, orphans): (_, Vec<_>) = {
                let mut state = multi.state.lock().unwrap();
                (
                    state.starting.take(),
                    state.bars.drain().map(|(_, bar)| bar).collect(),
                )
            };
            if let Some(progress) = starting {
                progress.finish_and_clear();
            }
            for bar in orphans {
                bar.finish_and_clear();
            }
        }
    }
}

impl Drop for DownloadProjectReporter {
    /// Backstop for error paths that bail before reaching [`Self::on_complete`]. The
    /// download loop may `return Err(_)` after calling `tasks.abort_all()`, which drops
    /// in-flight futures without running `on_download_complete` for their bars. Without
    /// this drop impl, the last `Arc<Self>` going away would leak those bars onto the
    /// terminal. All operations here are idempotent (they no-op if `on_complete` already
    /// ran on the success path).
    fn drop(&mut self) {
        self.on_complete();
    }
}

use std::env;
use std::fmt::Write;
use std::ops::Deref;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use rustc_hash::FxHashMap;

use crate::commands::human_readable_bytes;
use crate::printer::Printer;
use uv_cache::Removal;
use uv_distribution_filename::DistFilename;
use uv_distribution_types::{
    BuildableSource, CachedDist, DistributionMetadata, Name, SourceDist, VersionOrUrlRef,
};
use uv_normalize::PackageName;
use uv_pep440::Version;
use uv_python::PythonInstallationKey;
use uv_redacted::DisplaySafeUrl;
use uv_static::EnvVars;

/// Since downloads, fetches and builds run in parallel, their message output order is
/// non-deterministic, so can't capture them in test output.
static HAS_UV_TEST_NO_CLI_PROGRESS: LazyLock<bool> =
    LazyLock::new(|| env::var(EnvVars::UV_TEST_NO_CLI_PROGRESS).is_ok());

#[derive(Debug)]
struct ProgressReporter {
    printer: Printer,
    root: ProgressBar,
    mode: ProgressMode,
}

#[derive(Debug)]
enum ProgressMode {
    /// Reports top-level progress.
    Single,
    /// Reports progress of all concurrent download, build, and checkout processes.
    Multi {
        multi_progress: MultiProgress,
        state: Arc<Mutex<BarState>>,
    },
}

#[derive(Debug)]
enum ProgressBarKind {
    /// A progress bar with an increasing value, such as a download.
    Numeric {
        progress: ProgressBar,
        /// The download size in bytes, if known.
        size: Option<u64>,
    },
    /// A progress spinner for a task, such as a build.
    Spinner { progress: ProgressBar },
}

impl Deref for ProgressBarKind {
    type Target = ProgressBar;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Numeric { progress, .. } => progress,
            Self::Spinner { progress } => progress,
        }
    }
}

#[derive(Debug)]
struct BarState {
    /// The number of bars that precede any download bars (i.e., build/checkout status).
    headers: usize,
    /// A list of download bar sizes, in descending order.
    sizes: Vec<u64>,
    /// A map of progress bars, by ID.
    bars: FxHashMap<usize, ProgressBarKind>,
    /// A monotonic counter for bar IDs.
    id: usize,
    /// The maximum length of all bar names encountered.
    max_len: usize,
}

impl Default for BarState {
    fn default() -> Self {
        Self {
            headers: 0,
            sizes: Vec::default(),
            bars: FxHashMap::default(),
            id: 0,
            // Avoid resizing the progress bar templates too often by starting with a padding
            // that's wider than most package names.
            max_len: 20,
        }
    }
}

impl BarState {
    /// Returns a unique ID for a new progress bar.
    fn id(&mut self) -> usize {
        self.id += 1;
        self.id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Upload,
    Download,
    Extract,
    Hash,
}

impl Direction {
    fn as_str(&self) -> &str {
        match self {
            Self::Download => "Downloading",
            Self::Upload => "Uploading",
            Self::Extract => "Extracting",
            Self::Hash => "Hashing",
        }
    }
}

impl From<uv_python::downloads::Direction> for Direction {
    fn from(dir: uv_python::downloads::Direction) -> Self {
        match dir {
            uv_python::downloads::Direction::Download => Self::Download,
            uv_python::downloads::Direction::Extract => Self::Extract,
        }
    }
}

impl ProgressReporter {
    fn new(root: ProgressBar, multi_progress: MultiProgress, printer: Printer) -> Self {
        let mode = if env::var(EnvVars::JPY_SESSION_NAME).is_ok() {
            // Disable concurrent progress bars when running inside a Jupyter notebook
            // because the Jupyter terminal does not support clearing previous lines.
            // See: https://github.com/astral-sh/uv/issues/3887.
            ProgressMode::Single
        } else {
            ProgressMode::Multi {
                state: Arc::default(),
                multi_progress,
            }
        };

        Self {
            printer,
            root,
            mode,
        }
    }

    fn on_build_start(&self, source: &BuildableSource) -> usize {
        let ProgressMode::Multi {
            multi_progress,
            state,
        } = &self.mode
        else {
            return 0;
        };

        let mut state = state.lock().unwrap();
        let id = state.id();

        let progress = multi_progress.insert_before(
            &self.root,
            ProgressBar::with_draw_target(None, self.printer.target()),
        );

        progress.set_style(ProgressStyle::with_template("{wide_msg}").unwrap());
        let message = format!(
            "   {} {}",
            "Building".bold().cyan(),
            source.to_color_string()
        );
        if multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
            let _ = writeln!(self.printer.stderr(), "{message}");
        }
        progress.set_message(message);

        state.headers += 1;
        state.bars.insert(id, ProgressBarKind::Spinner { progress });
        id
    }

    fn on_build_complete(&self, source: &BuildableSource, id: usize) {
        let ProgressMode::Multi {
            state,
            multi_progress,
        } = &self.mode
        else {
            return;
        };

        let progress = {
            let mut state = state.lock().unwrap();
            state.headers -= 1;
            state.bars.remove(&id).unwrap()
        };

        let message = format!(
            "      {} {}",
            "Built".bold().green(),
            source.to_color_string()
        );
        if multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
            let _ = writeln!(self.printer.stderr(), "{message}");
        }
        progress.finish_with_message(message);
    }

    fn on_request_start(&self, direction: Direction, name: String, size: Option<u64>) -> usize {
        let ProgressMode::Multi {
            multi_progress,
            state,
        } = &self.mode
        else {
            return 0;
        };

        let mut state = state.lock().unwrap();

        // Preserve ascending order.
        let position = size.map_or(0, |size| state.sizes.partition_point(|&len| len < size));
        state.sizes.insert(position, size.unwrap_or(0));
        state.max_len = std::cmp::max(state.max_len, name.len());

        let max_len = state.max_len;
        for progress in state.bars.values_mut() {
            // Ignore spinners, such as for builds.
            if let ProgressBarKind::Numeric { progress, .. } = progress {
                let template = format!(
                    "{{msg:{max_len}.dim}} {{bar:30.green/black.dim}} {{binary_bytes:>7}}/{{binary_total_bytes:7}}"
                );
                progress.set_style(
                    ProgressStyle::with_template(&template)
                        .unwrap()
                        .progress_chars("--"),
                );
                progress.tick();
            }
        }

        let progress = multi_progress.insert(
            // Make sure not to reorder the initial "Preparing..." bar, or any previous bars.
            position + 1 + state.headers,
            ProgressBar::with_draw_target(size, self.printer.target()),
        );

        if let Some(size) = size {
            // We're using binary bytes to match `human_readable_bytes`.
            progress.set_style(
                ProgressStyle::with_template(
                    &format!(
                        "{{msg:{}.dim}} {{bar:30.green/black.dim}} {{binary_bytes:>7}}/{{binary_total_bytes:7}}", state.max_len
                    ),
                )
                    .unwrap()
                    .progress_chars("--"),
            );
            // If the file is larger than 1MB, show a message to indicate that this may take
            // a while keeping the log concise.
            if multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS && size > 1024 * 1024 {
                let (bytes, unit) = human_readable_bytes(size);
                let _ = writeln!(
                    self.printer.stderr(),
                    "{} {} {}",
                    direction.as_str().bold().cyan(),
                    name,
                    format!("({bytes:.1}{unit})").dimmed()
                );
            }
            progress.set_message(name);
        } else {
            progress.set_style(ProgressStyle::with_template("{wide_msg:.dim} ....").unwrap());
            if multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
                let _ = writeln!(
                    self.printer.stderr(),
                    "{} {}",
                    direction.as_str().bold().cyan(),
                    name
                );
            }
            progress.set_message(name);
            progress.finish();
        }

        let id = state.id();
        state
            .bars
            .insert(id, ProgressBarKind::Numeric { progress, size });
        id
    }

    fn on_request_progress(&self, id: usize, bytes: u64) {
        let ProgressMode::Multi { state, .. } = &self.mode else {
            return;
        };

        // Avoid panics due to reads on failed requests.
        // https://github.com/astral-sh/uv/issues/17090
        // TODO(konsti): Add a debug assert once https://github.com/seanmonstar/reqwest/issues/2884
        // is fixed
        if let Some(bar) = state.lock().unwrap().bars.get(&id) {
            bar.inc(bytes);
        }
    }

    fn on_request_complete(&self, direction: Direction, id: usize) {
        let ProgressMode::Multi {
            state,
            multi_progress,
        } = &self.mode
        else {
            return;
        };

        let mut state = state.lock().unwrap();
        if let ProgressBarKind::Numeric { progress, size } = state.bars.remove(&id).unwrap() {
            if multi_progress.is_hidden()
                && !*HAS_UV_TEST_NO_CLI_PROGRESS
                && size.is_none_or(|size| size > 1024 * 1024)
            {
                let _ = writeln!(
                    self.printer.stderr(),
                    " {} {}",
                    match direction {
                        Direction::Download => "Downloaded",
                        Direction::Upload => "Uploaded",
                        Direction::Extract => "Extracted",
                        Direction::Hash => "Hashed",
                    }
                    .bold()
                    .cyan(),
                    progress.message()
                );
            }
            progress.finish_and_clear();
        } else {
            debug_assert!(false, "Request progress bars are numeric");
        }
    }

    fn on_download_progress(&self, id: usize, bytes: u64) {
        self.on_request_progress(id, bytes);
    }

    fn on_download_complete(&self, id: usize) {
        self.on_request_complete(Direction::Download, id);
    }

    fn on_download_start(&self, name: String, size: Option<u64>) -> usize {
        self.on_request_start(Direction::Download, name, size)
    }

    fn on_upload_progress(&self, id: usize, bytes: u64) {
        self.on_request_progress(id, bytes);
    }

    fn on_upload_complete(&self, id: usize) {
        self.on_request_complete(Direction::Upload, id);
    }

    fn on_upload_start(&self, name: String, size: Option<u64>) -> usize {
        self.on_request_start(Direction::Upload, name, size)
    }

    fn on_hash_progress(&self, id: usize, bytes: u64) {
        self.on_request_progress(id, bytes);
    }

    fn on_hash_complete(&self, id: usize) {
        self.on_request_complete(Direction::Hash, id);
    }

    fn on_hash_start(&self, name: String, size: Option<u64>) -> usize {
        self.on_request_start(Direction::Hash, name, size)
    }

    fn on_checkout_start(&self, url: &DisplaySafeUrl, rev: &str) -> usize {
        let ProgressMode::Multi {
            multi_progress,
            state,
        } = &self.mode
        else {
            return 0;
        };

        let mut state = state.lock().unwrap();
        let id = state.id();

        let progress = multi_progress.insert_before(
            &self.root,
            ProgressBar::with_draw_target(None, self.printer.target()),
        );

        progress.set_style(ProgressStyle::with_template("{wide_msg}").unwrap());
        let message = format!("   {} {} ({})", "Updating".bold().cyan(), url, rev.dimmed());
        if multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
            let _ = writeln!(self.printer.stderr(), "{message}");
        }
        progress.set_message(message);
        progress.finish();

        state.headers += 1;
        state.bars.insert(id, ProgressBarKind::Spinner { progress });
        id
    }

    fn on_checkout_complete(&self, url: &DisplaySafeUrl, rev: &str, id: usize) {
        let ProgressMode::Multi {
            state,
            multi_progress,
        } = &self.mode
        else {
            return;
        };

        let progress = {
            let mut state = state.lock().unwrap();
            state.headers -= 1;
            state.bars.remove(&id).unwrap()
        };

        let message = format!(
            "    {} {} ({})",
            "Updated".bold().green(),
            url,
            rev.dimmed()
        );
        if multi_progress.is_hidden() && !*HAS_UV_TEST_NO_CLI_PROGRESS {
            let _ = writeln!(self.printer.stderr(), "{message}");
        }
        progress.finish_with_message(message);
    }
}

#[derive(Debug)]
pub(crate) struct PrepareReporter {
    reporter: ProgressReporter,
}

impl From<Printer> for PrepareReporter {
    fn from(printer: Printer) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());
        let root = multi_progress.add(ProgressBar::with_draw_target(None, printer.target()));
        root.enable_steady_tick(Duration::from_millis(200));
        root.set_style(
            ProgressStyle::with_template("{spinner:.white} {msg:.dim} ({pos}/{len})")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        root.set_message("Preparing packages...");

        let reporter = ProgressReporter::new(root, multi_progress, printer);
        Self { reporter }
    }
}

impl PrepareReporter {
    #[must_use]
    pub(crate) fn with_length(self, length: u64) -> Self {
        self.reporter.root.set_length(length);
        self
    }
}

impl uv_installer::PrepareReporter for PrepareReporter {
    fn on_progress(&self, _dist: &CachedDist) {
        self.reporter.root.inc(1);
    }

    fn on_complete(&self) {
        // Need an extra call to `set_message` here to fully clear avoid leaving ghost output
        // in Jupyter notebooks.
        self.reporter.root.set_message("");
        self.reporter.root.finish_and_clear();
    }

    fn on_build_start(&self, source: &BuildableSource) -> usize {
        self.reporter.on_build_start(source)
    }

    fn on_build_complete(&self, source: &BuildableSource, id: usize) {
        self.reporter.on_build_complete(source, id);
    }

    fn on_download_start(&self, name: &PackageName, size: Option<u64>) -> usize {
        self.reporter.on_download_start(name.to_string(), size)
    }

    fn on_download_progress(&self, id: usize, bytes: u64) {
        self.reporter.on_download_progress(id, bytes);
    }

    fn on_download_complete(&self, _name: &PackageName, id: usize) {
        self.reporter.on_download_complete(id);
    }

    fn on_checkout_start(&self, url: &DisplaySafeUrl, rev: &str) -> usize {
        self.reporter.on_checkout_start(url, rev)
    }

    fn on_checkout_complete(&self, url: &DisplaySafeUrl, rev: &str, id: usize) {
        self.reporter.on_checkout_complete(url, rev, id);
    }
}

#[derive(Debug)]
pub(crate) struct ResolverReporter {
    reporter: ProgressReporter,
}

impl ResolverReporter {
    #[must_use]
    pub(crate) fn with_length(self, length: u64) -> Self {
        self.reporter.root.set_length(length);
        self
    }
}

impl From<Printer> for ResolverReporter {
    fn from(printer: Printer) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());
        let root = multi_progress.add(ProgressBar::with_draw_target(None, printer.target()));
        root.enable_steady_tick(Duration::from_millis(200));
        root.set_style(
            ProgressStyle::with_template("{spinner:.white} {wide_msg:.dim}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        root.set_message("Resolving dependencies...");

        let reporter = ProgressReporter::new(root, multi_progress, printer);
        Self { reporter }
    }
}

impl uv_resolver::ResolverReporter for ResolverReporter {
    fn on_progress(&self, name: &PackageName, version_or_url: &VersionOrUrlRef) {
        match version_or_url {
            VersionOrUrlRef::Version(version) => {
                self.reporter.root.set_message(format!("{name}=={version}"));
            }
            VersionOrUrlRef::Url(url) => {
                self.reporter.root.set_message(format!("{name} @ {url}"));
            }
        }
    }

    fn on_complete(&self) {
        self.reporter.root.set_message("");
        self.reporter.root.finish_and_clear();
    }

    fn on_build_start(&self, source: &BuildableSource) -> usize {
        self.reporter.on_build_start(source)
    }

    fn on_build_complete(&self, source: &BuildableSource, id: usize) {
        self.reporter.on_build_complete(source, id);
    }

    fn on_checkout_start(&self, url: &DisplaySafeUrl, rev: &str) -> usize {
        self.reporter.on_checkout_start(url, rev)
    }

    fn on_checkout_complete(&self, url: &DisplaySafeUrl, rev: &str, id: usize) {
        self.reporter.on_checkout_complete(url, rev, id);
    }

    fn on_download_start(&self, name: &PackageName, size: Option<u64>) -> usize {
        self.reporter.on_download_start(name.to_string(), size)
    }

    fn on_download_progress(&self, id: usize, bytes: u64) {
        self.reporter.on_download_progress(id, bytes);
    }

    fn on_download_complete(&self, _name: &PackageName, id: usize) {
        self.reporter.on_download_complete(id);
    }
}

impl uv_distribution::Reporter for ResolverReporter {
    fn on_build_start(&self, source: &BuildableSource) -> usize {
        self.reporter.on_build_start(source)
    }

    fn on_build_complete(&self, source: &BuildableSource, id: usize) {
        self.reporter.on_build_complete(source, id);
    }

    fn on_download_start(&self, name: &PackageName, size: Option<u64>) -> usize {
        self.reporter.on_download_start(name.to_string(), size)
    }

    fn on_download_progress(&self, id: usize, bytes: u64) {
        self.reporter.on_download_progress(id, bytes);
    }

    fn on_download_complete(&self, _name: &PackageName, id: usize) {
        self.reporter.on_download_complete(id);
    }

    fn on_checkout_start(&self, url: &DisplaySafeUrl, rev: &str) -> usize {
        self.reporter.on_checkout_start(url, rev)
    }

    fn on_checkout_complete(&self, url: &DisplaySafeUrl, rev: &str, id: usize) {
        self.reporter.on_checkout_complete(url, rev, id);
    }
}

#[derive(Debug)]
pub(crate) struct InstallReporter {
    progress: ProgressBar,
}

impl From<Printer> for InstallReporter {
    fn from(printer: Printer) -> Self {
        let progress = ProgressBar::with_draw_target(None, printer.target());
        progress.set_style(
            ProgressStyle::with_template("{bar:20} [{pos}/{len}] {wide_msg:.dim}").unwrap(),
        );
        progress.set_message("Installing wheels...");
        Self { progress }
    }
}

impl InstallReporter {
    #[must_use]
    pub(crate) fn with_length(self, length: u64) -> Self {
        self.progress.set_length(length);
        self
    }
}

impl uv_installer::InstallReporter for InstallReporter {
    fn on_install_progress(&self, wheel: &CachedDist) {
        self.progress.set_message(format!("{wheel}"));
        self.progress.inc(1);
    }

    fn on_install_complete(&self) {
        self.progress.set_message("");
        self.progress.finish_and_clear();
    }
}

#[derive(Debug)]
pub(crate) struct PythonDownloadReporter {
    reporter: ProgressReporter,
}

impl PythonDownloadReporter {
    /// Initialize a [`PythonDownloadReporter`] for a single Python download.
    pub(crate) fn single(printer: Printer) -> Self {
        Self::new(printer, None)
    }

    /// Initialize a [`PythonDownloadReporter`] for multiple Python downloads.
    pub(crate) fn new(printer: Printer, length: Option<u64>) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());
        let root = multi_progress.add(ProgressBar::with_draw_target(length, printer.target()));
        let reporter = ProgressReporter::new(root, multi_progress, printer);
        Self { reporter }
    }
}

impl uv_python::downloads::Reporter for PythonDownloadReporter {
    fn on_request_start(
        &self,
        direction: uv_python::downloads::Direction,
        name: &PythonInstallationKey,
        size: Option<u64>,
    ) -> usize {
        self.reporter
            .on_request_start(direction.into(), format!("{name} ({direction})"), size)
    }

    fn on_request_progress(&self, id: usize, inc: u64) {
        self.reporter.on_request_progress(id, inc);
    }

    fn on_request_complete(&self, direction: uv_python::downloads::Direction, id: usize) {
        self.reporter.on_request_complete(direction.into(), id);
    }
}

/// Progress reporter for `uv download`, which streams wheel/sdist artifacts directly to
/// the output directory without going through the preparer.
///
/// A top-level "Downloading packages..." spinner with `(pos/len)` counter, plus per-artifact
/// byte-level sub-bars laid out on two lines (filename on one, bar + bytes on the next).
/// Wheel filenames with manylinux tags routinely exceed 90 characters, which overflows any
/// single-line template the moment a bar is appended — so the two-line layout is what keeps
/// the display readable regardless of terminal width.
#[derive(Debug)]
pub(crate) struct DownloadProjectReporter {
    printer: Printer,
    /// `None` in environments that do not render concurrent progress bars well (e.g. Jupyter).
    multi: Option<DownloadMulti>,
    root: ProgressBar,
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
}

impl DownloadProjectReporter {
    pub(crate) fn new(printer: Printer, length: u64) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());
        let root =
            multi_progress.add(ProgressBar::with_draw_target(Some(length), printer.target()));
        root.enable_steady_tick(Duration::from_millis(200));
        root.set_style(
            ProgressStyle::with_template("{spinner:.white} {msg:.dim} ({pos}/{len})")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        root.set_message("Downloading packages...");

        // Jupyter cannot redraw previous lines, so we fall back to the root spinner only.
        // See https://github.com/astral-sh/uv/issues/3887.
        let multi = if env::var(EnvVars::JPY_SESSION_NAME).is_ok() {
            None
        } else {
            Some(DownloadMulti {
                multi_progress,
                state: Arc::default(),
            })
        };

        Self {
            printer,
            multi,
            root,
        }
    }

    /// Open a byte-level bar for a single artifact download; call on HTTP 200 once
    /// `content_length` is known.
    pub(crate) fn on_download_start(&self, name: String, size: Option<u64>) -> usize {
        let Some(multi) = self.multi.as_ref() else {
            return 0;
        };

        let mut state = multi.state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;

        let progress = multi.multi_progress.insert_before(
            &self.root,
            ProgressBar::with_draw_target(size, self.printer.target()),
        );

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
                    "{wide_msg:.cyan}\n  {wide_bar:.green/dim} {binary_bytes:>10}/{binary_total_bytes:10}",
                )
                .unwrap()
                .progress_chars("--"),
            );
            if multi.multi_progress.is_hidden()
                && !*HAS_UV_TEST_NO_CLI_PROGRESS
                && size > 1024 * 1024
            {
                let (bytes, unit) = human_readable_bytes(size);
                let _ = writeln!(
                    self.printer.stderr(),
                    "{} {} {}",
                    "Downloading".bold().cyan(),
                    name,
                    format!("({bytes:.1}{unit})").dimmed()
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

        state.bars.insert(id, progress);
        id
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
    /// already materialized don't open a byte bar, so they must not call this. Root
    /// counter progression is handled separately by [`Self::on_task_done`] so that
    /// skipped-because-already-exists tasks still count toward `(pos/len)`.
    pub(crate) fn on_download_complete(&self, id: usize) {
        let Some(multi) = self.multi.as_ref() else {
            return;
        };
        let bar = multi.state.lock().unwrap().bars.remove(&id);
        if let Some(bar) = bar {
            bar.finish_and_clear();
        }
    }

    /// Tick the root `(pos/len)` counter — call once per finished task, regardless
    /// of whether it actually hit the network.
    pub(crate) fn on_task_done(&self) {
        self.root.inc(1);
    }

    /// Clear the root spinner once all downloads have finished.
    ///
    /// Also drains any sub-bars that are still registered — this matters when the outer
    /// download loop aborts in-flight tasks after a failure. Those futures are dropped
    /// at their `.await` point so `on_download_complete` never runs for them, leaving
    /// their bars drawn on the terminal until something clears them. Draining here
    /// guarantees the terminal is clean regardless of success or early abort.
    pub(crate) fn on_complete(&self) {
        if let Some(multi) = self.multi.as_ref() {
            let orphans: Vec<_> = {
                let mut state = multi.state.lock().unwrap();
                state.bars.drain().map(|(_, bar)| bar).collect()
            };
            for bar in orphans {
                bar.finish_and_clear();
            }
        }
        self.root.set_message("");
        self.root.finish_and_clear();
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

#[derive(Debug)]
pub(crate) struct PublishReporter {
    reporter: ProgressReporter,
}

impl PublishReporter {
    /// Initialize a [`PublishReporter`] for a single upload.
    pub(crate) fn single(printer: Printer) -> Self {
        Self::new(printer, None)
    }

    /// Initialize a [`PublishReporter`] for multiple uploads.
    pub(crate) fn new(printer: Printer, length: Option<u64>) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());
        let root = multi_progress.add(ProgressBar::with_draw_target(length, printer.target()));
        let reporter = ProgressReporter::new(root, multi_progress, printer);
        Self { reporter }
    }
}

impl uv_publish::Reporter for PublishReporter {
    fn on_progress(&self, _name: &str, id: usize) {
        self.reporter.on_download_complete(id);
    }

    fn on_upload_start(&self, name: &str, size: Option<u64>) -> usize {
        self.reporter.on_upload_start(name.to_string(), size)
    }

    fn on_upload_progress(&self, id: usize, inc: u64) {
        self.reporter.on_upload_progress(id, inc);
    }

    fn on_upload_complete(&self, id: usize) {
        self.reporter.on_upload_complete(id);
    }

    fn on_hash_start(&self, name: &DistFilename, size: Option<u64>) -> usize {
        self.reporter.on_hash_start(name.to_string(), size)
    }

    fn on_hash_progress(&self, id: usize, inc: u64) {
        self.reporter.on_hash_progress(id, inc);
    }

    fn on_hash_complete(&self, id: usize) {
        self.reporter.on_hash_complete(id);
    }
}

#[derive(Debug)]
pub(crate) struct LatestVersionReporter {
    progress: ProgressBar,
}

impl From<Printer> for LatestVersionReporter {
    fn from(printer: Printer) -> Self {
        let progress = ProgressBar::with_draw_target(None, printer.target());
        progress.set_style(
            ProgressStyle::with_template("{bar:20} [{pos}/{len}] {wide_msg:.dim}").unwrap(),
        );
        progress.set_message("Fetching latest versions...");
        Self { progress }
    }
}

impl LatestVersionReporter {
    #[must_use]
    pub(crate) fn with_length(self, length: u64) -> Self {
        self.progress.set_length(length);
        self
    }

    pub(crate) fn on_fetch_progress(&self) {
        self.progress.inc(1);
    }

    pub(crate) fn on_fetch_version(&self, name: &PackageName, version: &Version) {
        self.progress.set_message(format!("{name} v{version}"));
        self.progress.inc(1);
    }

    pub(crate) fn on_fetch_complete(&self) {
        self.progress.set_message("");
        self.progress.finish_and_clear();
    }
}

#[derive(Debug)]
pub(crate) struct AuditReporter {
    progress: ProgressBar,
}

impl From<Printer> for AuditReporter {
    fn from(printer: Printer) -> Self {
        let progress = ProgressBar::with_draw_target(None, printer.target());
        progress.enable_steady_tick(Duration::from_millis(200));
        progress.set_style(
            ProgressStyle::with_template("{spinner:.white} {wide_msg:.dim}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        progress.set_message("Auditing dependencies...");
        Self { progress }
    }
}

impl AuditReporter {
    pub(crate) fn on_audit_complete(&self) {
        self.progress.set_message("");
        self.progress.finish_and_clear();
    }
}

#[derive(Debug)]
pub(crate) struct CleaningDirectoryReporter {
    bar: ProgressBar,
}

impl CleaningDirectoryReporter {
    /// Initialize a [`CleaningDirectoryReporter`] for cleaning the cache directory.
    pub(crate) fn new(printer: Printer, max: Option<usize>) -> Self {
        let bar = ProgressBar::with_draw_target(max.map(|m| m as u64), printer.target());
        bar.set_style(
            ProgressStyle::with_template("{prefix} [{bar:20}] {percent}%")
                .unwrap()
                .progress_chars("=> "),
        );
        bar.set_prefix(format!("{}", "Cleaning".bold().cyan()));
        Self { bar }
    }
}

impl uv_cache::CleanReporter for CleaningDirectoryReporter {
    fn on_clean(&self) {
        self.bar.inc(1);
    }

    fn on_complete(&self) {
        self.bar.finish_and_clear();
    }
}

#[derive(Debug)]
pub(crate) struct CleaningPackageReporter {
    bar: ProgressBar,
}

impl CleaningPackageReporter {
    /// Initialize a [`CleaningPackageReporter`] for cleaning packages from the cache.
    pub(crate) fn new(printer: Printer, max: Option<usize>) -> Self {
        let bar = ProgressBar::with_draw_target(max.map(|m| m as u64), printer.target());
        bar.set_style(
            ProgressStyle::with_template("{prefix} [{bar:20}] {pos}/{len}{msg}")
                .unwrap()
                .progress_chars("=> "),
        );
        bar.set_prefix(format!("{}", "Cleaning".bold().cyan()));
        Self { bar }
    }

    pub(crate) fn on_clean(&self, package: &str, removal: &Removal) {
        self.bar.inc(1);
        self.bar.set_message(format!(
            ": {}, {} files {} folders removed",
            package, removal.num_files, removal.num_dirs,
        ));
    }

    pub(crate) fn on_complete(&self) {
        self.bar.finish_and_clear();
    }
}

/// Like [`std::fmt::Display`], but with colors.
trait ColorDisplay {
    fn to_color_string(&self) -> String;
}

impl ColorDisplay for SourceDist {
    fn to_color_string(&self) -> String {
        let name = self.name();
        let version_or_url = self.version_or_url();
        format!("{}{}", name, version_or_url.to_string().dimmed())
    }
}

impl ColorDisplay for BuildableSource<'_> {
    fn to_color_string(&self) -> String {
        match self {
            Self::Dist(dist) => dist.to_color_string(),
            Self::Url(url) => url.to_string(),
        }
    }
}

pub(crate) struct BinaryDownloadReporter {
    reporter: ProgressReporter,
}

impl BinaryDownloadReporter {
    /// Initialize a [`BinaryDownloadReporter`] for a single binary download.
    pub(crate) fn single(printer: Printer) -> Self {
        let multi_progress = MultiProgress::with_draw_target(printer.target());
        let root = multi_progress.add(ProgressBar::with_draw_target(None, printer.target()));
        let reporter = ProgressReporter::new(root, multi_progress, printer);
        Self { reporter }
    }
}

impl uv_bin_install::Reporter for BinaryDownloadReporter {
    fn on_download_start(&self, name: &str, version: &Version, size: Option<u64>) -> usize {
        self.reporter
            .on_request_start(Direction::Download, format!("{name} v{version}"), size)
    }

    fn on_download_progress(&self, id: usize, inc: u64) {
        self.reporter.on_request_progress(id, inc);
    }

    fn on_download_complete(&self, id: usize) {
        self.reporter.on_request_complete(Direction::Download, id);
    }
}

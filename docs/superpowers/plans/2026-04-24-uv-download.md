# `uv download` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new top-level `uv download` subcommand that resolves a project's `uv.lock` for a
target platform/machine/glibc/implementation and writes the resolved wheels and sdists into
`--output-dir`, without creating a virtual environment or installing anything.

**Architecture:** New command reuses `uv sync`'s lock / resolve / preparer pipeline but replaces the
installation phase with file materialization into an output directory. A new `PlatformSpec` helper
in `uv-configuration` normalizes user-facing flags (`linux|windows|macos`,
`x86_64|AMD64|aarch64|arm64|...`, glibc `MAJOR.MINOR`, `CPython`) into an existing `TargetTriple`. A
new `operations::prepare` helper is extracted from `operations::install` so both commands share the
download path.

**Tech Stack:** Rust 2024 edition, `clap` 4.x (derive), `insta` snapshot tests, `assert_cmd`,
`assert_fs`, `anyhow`, existing uv crates (`uv-cli`, `uv-configuration`, `uv-platform-tags`,
`uv-resolver`, `uv-installer`, `uv-test`).

**Reference spec:** `docs/superpowers/specs/2026-04-24-uv-download-design.md`.

---

## Ground rules

- Work in the current branch on top of `main`.
- `cargo clippy` (not `cargo check`) for local verification.
- No `.unwrap()`, `panic!()`, `unreachable!()`, `unsafe`, or clippy allow-lists; prefer `if let` and
  let-chains.
- Run narrow tests: `cargo nextest run -p uv --test it -- download::<name>` or
  `cargo test -p uv-configuration platform_spec::`.
- Windows cross-check: after any task that changes parsing or platform mapping, run
  `cargo xwin clippy -p uv-configuration -p uv-cli -p uv`.
- Commit after every task. Use present-tense imperative subjects, no trailing Co-Authored-By line
  (uv commit style).

---

## Task 1: PlatformSpec foundation — unit tests first

**Files:**

- Create: `crates/uv-configuration/src/platform_spec.rs`
- Modify: `crates/uv-configuration/src/lib.rs`

- [ ] **Step 1: Create failing unit tests**

Write `crates/uv-configuration/src/platform_spec.rs` with a `#[cfg(test)] mod tests` section
containing the full expected behavior. The test section must compile but fail because the types do
not exist yet.

```rust
use std::path::PathBuf;
use std::str::FromStr;

use uv_platform_tags::Arch;

use crate::TargetTriple;

// ---------------------------------------------------------------- Public API

/// User-facing OS value accepted by `uv download --platform`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PlatformOs {
    Linux,
    Windows,
    Macos,
}

impl FromStr for PlatformOs {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "linux" => Ok(Self::Linux),
            "windows" | "win32" => Ok(Self::Windows),
            "macos" | "darwin" | "osx" => Ok(Self::Macos),
            other => Err(format!(
                "unknown platform `{other}`; expected one of `linux`, `windows`, `macos`"
            )),
        }
    }
}

/// User-facing Python implementation accepted by `uv download --implementation`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PyImpl {
    CPython,
}

impl FromStr for PyImpl {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cpython" | "cp" => Ok(Self::CPython),
            other => Err(format!(
                "unsupported Python implementation `{other}`; only `CPython` is supported"
            )),
        }
    }
}

/// Parsed and normalized architecture input.
pub fn parse_machine(raw: &str) -> Result<Arch, String> {
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "amd64" | "x86_64" | "x86-64" | "x64" => Ok(Arch::X86_64),
        "arm64" | "aarch64" => Ok(Arch::Aarch64),
        "i386" | "i686" | "x86" => Ok(Arch::X86),
        other => Arch::from_str(other).map_err(|_| {
            format!(
                "unknown machine `{raw}`; expected one of `x86_64`, `amd64`, `aarch64`, `arm64`, `i686`, …"
            )
        }),
    }
}

/// Parse `MAJOR.MINOR` or `MAJOR_MINOR` into `(major, minor)`.
pub fn parse_glibc(raw: &str) -> Result<(u16, u16), String> {
    let trimmed = raw.trim();
    let parts: Vec<&str> = if trimmed.contains('.') {
        trimmed.split('.').collect()
    } else if trimmed.contains('_') {
        trimmed.split('_').collect()
    } else {
        return Err(format!(
            "invalid glibc version `{raw}`; expected `MAJOR.MINOR` (e.g. `2.28`)"
        ));
    };
    if parts.len() != 2 {
        return Err(format!(
            "invalid glibc version `{raw}`; expected `MAJOR.MINOR` (e.g. `2.28`)"
        ));
    }
    let major = parts[0]
        .parse::<u16>()
        .map_err(|_| format!("invalid glibc major `{}`", parts[0]))?;
    let minor = parts[1]
        .parse::<u16>()
        .map_err(|_| format!("invalid glibc minor `{}`", parts[1]))?;
    Ok((major, minor))
}

/// Structured, normalized platform input for `uv download`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PlatformSpec {
    pub os: PlatformOs,
    pub arch: Arch,
    pub glibc: Option<(u16, u16)>,
    pub implementation: PyImpl,
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformSpecError {
    #[error("--glibc is only valid with --platform=linux")]
    GlibcOnNonLinux,
    #[error("only CPython is supported in this release")]
    UnsupportedImplementation,
    #[error("{os:?} + {arch} is not a supported target platform")]
    UnsupportedCombination { os: PlatformOs, arch: Arch },
    #[error(
        "manylinux_2_{minor} is not supported for {arch} (supported minors: {supported:?})"
    )]
    UnsupportedGlibc {
        arch: Arch,
        minor: u16,
        supported: &'static [u16],
    },
}

impl PlatformSpec {
    /// Build a `PlatformSpec` from already-parsed CLI inputs, applying defaults.
    pub fn from_parts(
        os: Option<PlatformOs>,
        arch: Option<Arch>,
        glibc: Option<(u16, u16)>,
        implementation: Option<PyImpl>,
        host_os: PlatformOs,
        host_arch: Arch,
    ) -> Result<Self, PlatformSpecError> {
        let os = os.unwrap_or(host_os);
        let arch = arch.unwrap_or(host_arch);
        let implementation = implementation.unwrap_or(PyImpl::CPython);
        if implementation != PyImpl::CPython {
            return Err(PlatformSpecError::UnsupportedImplementation);
        }
        if glibc.is_some() && os != PlatformOs::Linux {
            return Err(PlatformSpecError::GlibcOnNonLinux);
        }
        let glibc = if os == PlatformOs::Linux {
            Some(glibc.unwrap_or((2, 28)))
        } else {
            None
        };
        Ok(Self {
            os,
            arch,
            glibc,
            implementation,
        })
    }

    /// Map the normalized spec to an existing uv `TargetTriple`.
    pub fn to_target_triple(self) -> Result<TargetTriple, PlatformSpecError> {
        use Arch::*;
        use PlatformOs::*;
        match (self.os, self.arch, self.glibc) {
            (Linux, X86_64, Some((2, minor))) => match minor {
                17 => Ok(TargetTriple::X8664Manylinux217),
                28 => Ok(TargetTriple::X8664Manylinux228),
                31 => Ok(TargetTriple::X8664Manylinux231),
                32 => Ok(TargetTriple::X8664Manylinux232),
                33 => Ok(TargetTriple::X8664Manylinux233),
                34 => Ok(TargetTriple::X8664Manylinux234),
                35 => Ok(TargetTriple::X8664Manylinux235),
                36 => Ok(TargetTriple::X8664Manylinux236),
                37 => Ok(TargetTriple::X8664Manylinux237),
                38 => Ok(TargetTriple::X8664Manylinux238),
                39 => Ok(TargetTriple::X8664Manylinux239),
                40 => Ok(TargetTriple::X8664Manylinux240),
                _ => Err(PlatformSpecError::UnsupportedGlibc {
                    arch: X86_64,
                    minor,
                    supported: &[
                        17, 28, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
                    ],
                }),
            },
            (Linux, Aarch64, Some((2, minor))) => match minor {
                17 => Ok(TargetTriple::Aarch64Manylinux217),
                28 => Ok(TargetTriple::Aarch64Manylinux228),
                31 => Ok(TargetTriple::Aarch64Manylinux231),
                32 => Ok(TargetTriple::Aarch64Manylinux232),
                33 => Ok(TargetTriple::Aarch64Manylinux233),
                34 => Ok(TargetTriple::Aarch64Manylinux234),
                35 => Ok(TargetTriple::Aarch64Manylinux235),
                36 => Ok(TargetTriple::Aarch64Manylinux236),
                37 => Ok(TargetTriple::Aarch64Manylinux237),
                38 => Ok(TargetTriple::Aarch64Manylinux238),
                39 => Ok(TargetTriple::Aarch64Manylinux239),
                40 => Ok(TargetTriple::Aarch64Manylinux240),
                _ => Err(PlatformSpecError::UnsupportedGlibc {
                    arch: Aarch64,
                    minor,
                    supported: &[
                        17, 28, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
                    ],
                }),
            },
            (Linux, Riscv64, _) => Ok(TargetTriple::Riscv64UnknownLinuxGnu),
            (Windows, X86_64, None) => Ok(TargetTriple::X8664PcWindowsMsvc),
            (Windows, Aarch64, None) => Ok(TargetTriple::Aarch64PcWindowsMsvc),
            (Windows, X86, None) => Ok(TargetTriple::I686PcWindowsMsvc),
            (Macos, X86_64, None) => Ok(TargetTriple::X8664AppleDarwin),
            (Macos, Aarch64, None) => Ok(TargetTriple::Aarch64AppleDarwin),
            (os, arch, _) => Err(PlatformSpecError::UnsupportedCombination { os, arch }),
        }
    }
}

// --------------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> (PlatformOs, Arch) {
        (PlatformOs::Linux, Arch::X86_64)
    }

    #[test]
    fn os_from_str_normalizes_case_and_aliases() {
        for good in ["linux", "Linux", "LINUX"] {
            assert_eq!(good.parse::<PlatformOs>(), Ok(PlatformOs::Linux));
        }
        for good in ["windows", "Windows", "WINDOWS", "win32", "Win32"] {
            assert_eq!(good.parse::<PlatformOs>(), Ok(PlatformOs::Windows));
        }
        for good in ["macos", "MacOS", "darwin", "Darwin", "osx"] {
            assert_eq!(good.parse::<PlatformOs>(), Ok(PlatformOs::Macos));
        }
        assert!("solaris".parse::<PlatformOs>().is_err());
    }

    #[test]
    fn machine_parses_aliases() {
        for good in ["x86_64", "X86_64", "amd64", "AMD64", "x86-64", "x64"] {
            assert_eq!(parse_machine(good), Ok(Arch::X86_64));
        }
        for good in ["aarch64", "AARCH64", "arm64", "ARM64"] {
            assert_eq!(parse_machine(good), Ok(Arch::Aarch64));
        }
        for good in ["i686", "i386", "x86"] {
            assert_eq!(parse_machine(good), Ok(Arch::X86));
        }
        assert!(parse_machine("mips").is_err());
    }

    #[test]
    fn glibc_parses_dot_and_underscore() {
        assert_eq!(parse_glibc("2.28"), Ok((2, 28)));
        assert_eq!(parse_glibc("2_28"), Ok((2, 28)));
        assert!(parse_glibc("2").is_err());
        assert!(parse_glibc("2.2.2").is_err());
        assert!(parse_glibc("two.two").is_err());
    }

    #[test]
    fn implementation_defaults_and_rejects_non_cpython() {
        let (host_os, host_arch) = host();
        let spec = PlatformSpec::from_parts(None, None, None, None, host_os, host_arch).unwrap();
        assert_eq!(spec.implementation, PyImpl::CPython);
        assert!("pypy".parse::<PyImpl>().is_err());
    }

    #[test]
    fn glibc_on_windows_is_rejected() {
        let err = PlatformSpec::from_parts(
            Some(PlatformOs::Windows),
            Some(Arch::X86_64),
            Some((2, 28)),
            None,
            PlatformOs::Linux,
            Arch::X86_64,
        )
        .unwrap_err();
        assert!(matches!(err, PlatformSpecError::GlibcOnNonLinux));
    }

    #[test]
    fn glibc_on_macos_is_rejected() {
        let err = PlatformSpec::from_parts(
            Some(PlatformOs::Macos),
            Some(Arch::Aarch64),
            Some((2, 28)),
            None,
            PlatformOs::Linux,
            Arch::X86_64,
        )
        .unwrap_err();
        assert!(matches!(err, PlatformSpecError::GlibcOnNonLinux));
    }

    #[test]
    fn defaults_glibc_to_2_28_on_linux() {
        let (host_os, host_arch) = host();
        let spec = PlatformSpec::from_parts(
            Some(PlatformOs::Linux),
            Some(Arch::Aarch64),
            None,
            None,
            host_os,
            host_arch,
        )
        .unwrap();
        assert_eq!(spec.glibc, Some((2, 28)));
    }

    #[test]
    fn host_defaults_apply_when_unset() {
        let spec = PlatformSpec::from_parts(
            None,
            None,
            None,
            None,
            PlatformOs::Macos,
            Arch::Aarch64,
        )
        .unwrap();
        assert_eq!(spec.os, PlatformOs::Macos);
        assert_eq!(spec.arch, Arch::Aarch64);
        assert_eq!(spec.glibc, None);
    }

    #[test]
    fn target_triple_matrix_linux_x86_64() {
        for (minor, expected) in [
            (17, TargetTriple::X8664Manylinux217),
            (28, TargetTriple::X8664Manylinux228),
            (31, TargetTriple::X8664Manylinux231),
            (40, TargetTriple::X8664Manylinux240),
        ] {
            let spec = PlatformSpec {
                os: PlatformOs::Linux,
                arch: Arch::X86_64,
                glibc: Some((2, minor)),
                implementation: PyImpl::CPython,
            };
            assert_eq!(spec.to_target_triple(), Ok(expected));
        }
    }

    #[test]
    fn target_triple_matrix_linux_aarch64() {
        let spec = PlatformSpec {
            os: PlatformOs::Linux,
            arch: Arch::Aarch64,
            glibc: Some((2, 28)),
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            spec.to_target_triple(),
            Ok(TargetTriple::Aarch64Manylinux228)
        );
    }

    #[test]
    fn target_triple_matrix_windows_and_macos() {
        let win = PlatformSpec {
            os: PlatformOs::Windows,
            arch: Arch::X86_64,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(win.to_target_triple(), Ok(TargetTriple::X8664PcWindowsMsvc));

        let mac = PlatformSpec {
            os: PlatformOs::Macos,
            arch: Arch::Aarch64,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            mac.to_target_triple(),
            Ok(TargetTriple::Aarch64AppleDarwin)
        );
    }

    #[test]
    fn target_triple_unsupported_combination() {
        let spec = PlatformSpec {
            os: PlatformOs::Macos,
            arch: Arch::X86,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert!(matches!(
            spec.to_target_triple(),
            Err(PlatformSpecError::UnsupportedCombination { .. })
        ));
    }

    #[test]
    fn target_triple_unsupported_glibc_minor() {
        let spec = PlatformSpec {
            os: PlatformOs::Linux,
            arch: Arch::X86_64,
            glibc: Some((2, 5)),
            implementation: PyImpl::CPython,
        };
        assert!(matches!(
            spec.to_target_triple(),
            Err(PlatformSpecError::UnsupportedGlibc { .. })
        ));
    }
}
```

Keep the unused `PathBuf` import removed by the formatter — if clippy flags it, delete the line.

- [ ] **Step 2: Wire the module into the crate**

Edit `crates/uv-configuration/src/lib.rs`. Add the module declaration and re-exports next to the
existing ones (search for the existing `pub mod target_triple;` and add below):

```rust
pub mod platform_spec;

pub use platform_spec::{
    PlatformOs, PlatformSpec, PlatformSpecError, PyImpl, parse_glibc, parse_machine,
};
```

- [ ] **Step 3: Confirm the module compiles and tests fail**

Run: `cargo test -p uv-configuration platform_spec --no-run` Expected: build succeeds, tests
compile.

Run: `cargo test -p uv-configuration platform_spec` Expected: all tests listed in Step 1 pass (the
implementation was written alongside).

If any test fails due to formatter-introduced changes, read the file, correct inline, re-run.

- [ ] **Step 4: Clippy the crate**

Run: `cargo clippy -p uv-configuration --all-targets -- -D warnings` Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/uv-configuration/src/platform_spec.rs crates/uv-configuration/src/lib.rs
git commit -m "Add PlatformSpec for uv download input normalization"
```

---

## Task 2: Extract `operations::prepare` from `operations::install`

**Files:**

- Modify: `crates/uv/src/commands/pip/operations.rs`

This is a pure refactor: move the `Preparer` invocation out of `execute_plan` into a reusable
`pub(crate) async fn prepare(...)` so both install and download can call it. Behavior must not
change — all existing sync / pip tests remain the source of truth.

- [ ] **Step 1: Add the new `prepare` helper**

In `crates/uv/src/commands/pip/operations.rs`, add (near `execute_plan`):

```rust
/// Download / build / unzip the given distributions into the cache.
///
/// Returns a `Vec<CachedDist>` pointing at on-disk artifacts. Used by both
/// `operations::install` (during sync) and `commands::project::download`.
pub(crate) async fn prepare(
    dists: Vec<Dist>,
    in_flight: &InFlight,
    resolution: &Resolution,
    hasher: &HashStrategy,
    tags: &Tags,
    build_options: &BuildOptions,
    client: &RegistryClient,
    build_dispatch: &BuildDispatch<'_>,
    cache: &Cache,
    concurrency: &Concurrency,
    printer: Printer,
) -> Result<Vec<CachedDist>, Error> {
    if dists.is_empty() {
        return Ok(Vec::new());
    }
    let preparer = Preparer::new(
        cache,
        tags,
        hasher,
        build_options,
        DistributionDatabase::new(
            client,
            build_dispatch,
            concurrency.downloads_semaphore.clone(),
        ),
    )
    .with_reporter(Arc::new(
        PrepareReporter::from(printer).with_length(dists.len() as u64),
    ));

    preparer.prepare(dists, in_flight, resolution).await
}
```

- [ ] **Step 2: Re-point `execute_plan` at the new helper**

Replace the block starting at `let wheels = if remote.is_empty() {` inside `execute_plan` with:

```rust
let wheels = if remote.is_empty() {
    Vec::new()
} else {
    let start = std::time::Instant::now();
    let wheels = prepare(
        remote.clone(),
        in_flight,
        resolution,
        hasher,
        tags,
        build_options,
        client,
        build_dispatch,
        cache,
        concurrency,
        printer,
    )
    .await?;
    logger.on_prepare(
        wheels.len(),
        phase.map(InstallPhase::label),
        start,
        printer,
        DryRun::Disabled,
    )?;
    wheels
};
```

Remove the now-unused local `Preparer::new(...)` block inside `execute_plan`.

- [ ] **Step 3: Verify behavior is unchanged**

Run: `cargo test -p uv --test it sync::sync -- --nocapture` Expected: `sync` basic tests (at least
`sync` and `locked`) pass.

Run: `cargo test -p uv --test it pip_install::install_command` Expected: passes.

- [ ] **Step 4: Clippy**

Run: `cargo clippy -p uv --all-targets -- -D warnings` Expected: clean. If the `Dist` / `CachedDist`
imports at the top of the file need adjustment, bring them in at crate root (top-level imports, not
local).

- [ ] **Step 5: Commit**

```bash
git add crates/uv/src/commands/pip/operations.rs
git commit -m "Extract operations::prepare so it can be reused for download"
```

---

## Task 3: Add `DownloadArgs` CLI surface

**Files:**

- Modify: `crates/uv-cli/src/lib.rs`

- [ ] **Step 1: Declare the variant on `Commands`**

Search for `Sync(SyncArgs),` in `crates/uv-cli/src/lib.rs` and add immediately after it:

```rust
/// Download the project's dependencies into a directory without installing them.
///
/// Resolves `uv.lock` for the requested target platform and writes all wheels
/// (and any sdists referenced by the lockfile) into `--output-dir`.
///
/// Does not create or modify a virtual environment. Local path, editable, and
/// workspace-member dependencies are skipped with a warning.
#[command(
    after_help = "Use `uv help download` for more details.",
    after_long_help = ""
)]
Download(Box<DownloadArgs>),
```

- [ ] **Step 2: Define `DownloadArgs`**

Add near `SyncArgs` (after its closing brace):

```rust
#[derive(Args)]
pub struct DownloadArgs {
    /// The target operating system: `linux`, `windows`, or `macos`.
    ///
    /// Case-insensitive. Accepts aliases `win32` for Windows and
    /// `darwin` / `osx` for macOS. Defaults to the current host OS.
    #[arg(long, value_parser = parse_platform_os)]
    pub platform: Option<uv_configuration::PlatformOs>,

    /// The target machine/architecture: `x86_64`, `aarch64`, `i686`, ...
    ///
    /// Case-insensitive. Accepts aliases `amd64`/`x64` for `x86_64` and
    /// `arm64` for `aarch64`. Defaults to the current host machine.
    #[arg(long, value_parser = parse_platform_machine)]
    pub machine: Option<uv_platform_tags::Arch>,

    /// The minimum glibc version, as `MAJOR.MINOR` (e.g. `2.28`).
    ///
    /// Only valid with `--platform=linux`. Defaults to `2.28`.
    #[arg(long, value_parser = parse_platform_glibc)]
    pub glibc: Option<(u16, u16)>,

    /// The Python implementation. Only `CPython` is supported today.
    #[arg(long, alias = "impl", value_parser = parse_platform_implementation, default_value = "CPython")]
    pub implementation: uv_configuration::PyImpl,

    /// The directory to write wheels / sdists into.
    ///
    /// Created if missing. If the directory already contains artifacts with
    /// the same filename, they are left untouched.
    #[arg(short = 'o', long = "output-dir", alias = "out", value_hint = ValueHint::DirPath)]
    pub output_dir: PathBuf,

    /// Include optional dependencies from the specified extra name.
    #[arg(long, conflicts_with = "all_extras", value_delimiter = ',', value_parser = extra_name_with_clap_error, value_hint = ValueHint::Other)]
    pub extra: Option<Vec<ExtraName>>,

    #[arg(long, conflicts_with = "extra")]
    pub all_extras: bool,

    #[arg(long, value_hint = ValueHint::Other)]
    pub no_extra: Vec<ExtraName>,

    #[arg(long, conflicts_with_all = ["only_group", "only_dev"], value_hint = ValueHint::Other)]
    pub group: Vec<GroupName>,

    #[arg(long, env = EnvVars::UV_NO_GROUP, value_delimiter = ' ', value_hint = ValueHint::Other)]
    pub no_group: Vec<GroupName>,

    #[arg(long, env = EnvVars::UV_NO_DEFAULT_GROUPS, value_parser = clap::builder::BoolishValueParser::new())]
    pub no_default_groups: bool,

    #[arg(long, conflicts_with_all = ["group", "dev", "all_groups"], value_hint = ValueHint::Other)]
    pub only_group: Vec<GroupName>,

    #[arg(long, conflicts_with_all = ["only_group", "only_dev"])]
    pub all_groups: bool,

    #[arg(long, overrides_with("no_dev"), hide = true, value_parser = clap::builder::BoolishValueParser::new())]
    pub dev: bool,

    #[arg(long, overrides_with("dev"), value_parser = clap::builder::BoolishValueParser::new())]
    pub no_dev: bool,

    #[arg(long, conflicts_with_all = ["group", "all_groups", "no_dev"])]
    pub only_dev: bool,

    /// Assert that the `uv.lock` will remain unchanged.
    #[arg(long, conflicts_with_all = ["frozen", "upgrade"])]
    pub locked: bool,

    /// Download without updating `uv.lock`.
    #[arg(long, conflicts_with_all = ["locked", "upgrade", "no_sources"])]
    pub frozen: bool,

    #[command(flatten)]
    pub installer: ResolverInstallerArgs,

    #[command(flatten)]
    pub build: BuildOptionsArgs,

    #[command(flatten)]
    pub refresh: RefreshArgs,

    /// The Python interpreter (used only to derive tags/markers; no venv is created).
    #[arg(
        long,
        short,
        env = EnvVars::UV_PYTHON,
        verbatim_doc_comment,
        help_heading = "Python options",
        value_parser = parse_maybe_string,
        value_hint = ValueHint::Other,
    )]
    pub python: Option<Maybe<String>>,
}
```

- [ ] **Step 3: Add the free-function value parsers**

At the bottom of the file (or near the other `parse_maybe_*` helpers), add:

```rust
fn parse_platform_os(raw: &str) -> Result<uv_configuration::PlatformOs, String> {
    raw.parse()
}

fn parse_platform_machine(raw: &str) -> Result<uv_platform_tags::Arch, String> {
    uv_configuration::parse_machine(raw)
}

fn parse_platform_glibc(raw: &str) -> Result<(u16, u16), String> {
    uv_configuration::parse_glibc(raw)
}

fn parse_platform_implementation(raw: &str) -> Result<uv_configuration::PyImpl, String> {
    raw.parse()
}
```

- [ ] **Step 4: Verify compilation**

Run: `cargo check -p uv-cli`

Wait — we do not use `cargo check` in this repo. Use:

Run: `cargo clippy -p uv-cli --all-targets -- -D warnings` Expected: compiles cleanly.

- [ ] **Step 5: Commit**

```bash
git add crates/uv-cli/src/lib.rs
git commit -m "Add `uv download` CLI arguments"
```

---

## Task 4: Add `DownloadSettings`

**Files:**

- Modify: `crates/uv/src/settings.rs`

- [ ] **Step 1: Add `DownloadSettings`**

Locate the `SyncSettings` definition in `crates/uv/src/settings.rs` and add below it:

```rust
#[derive(Debug, Clone)]
pub(crate) struct DownloadSettings {
    pub(crate) locked: bool,
    pub(crate) frozen: bool,
    pub(crate) extras: ExtrasSpecification,
    pub(crate) groups: DependencyGroups,
    pub(crate) output_dir: PathBuf,
    pub(crate) platform: uv_configuration::PlatformOs,
    pub(crate) machine: uv_platform_tags::Arch,
    pub(crate) glibc: Option<(u16, u16)>,
    pub(crate) implementation: uv_configuration::PyImpl,
    pub(crate) python: Option<String>,
    pub(crate) install_mirrors: PythonInstallMirrors,
    pub(crate) settings: ResolverInstallerSettings,
    pub(crate) refresh: Refresh,
}

impl DownloadSettings {
    /// Resolve [`DownloadSettings`] from CLI args plus the filesystem / environment stack.
    pub(crate) fn resolve(
        args: uv_cli::DownloadArgs,
        filesystem: Option<FilesystemOptions>,
        environment: EnvironmentOptions,
    ) -> Self {
        let uv_cli::DownloadArgs {
            platform,
            machine,
            glibc,
            implementation,
            output_dir,
            extra,
            all_extras,
            no_extra,
            group,
            no_group,
            no_default_groups,
            only_group,
            all_groups,
            dev,
            no_dev,
            only_dev,
            locked,
            frozen,
            installer,
            build,
            refresh,
            python,
        } = args;

        let (host_os, host_arch) = host_platform_machine();
        let platform = platform.unwrap_or(host_os);
        let machine = machine.unwrap_or(host_arch);

        let extras = ExtrasSpecification::from_args(all_extras, no_extra, extra.unwrap_or_default());
        let groups = DependencyGroups::from_args(
            dev,
            no_dev,
            only_dev,
            group,
            no_group,
            no_default_groups,
            only_group,
            all_groups,
        );

        let install_mirrors = filesystem
            .as_ref()
            .and_then(|fs| fs.install_mirrors.as_ref())
            .cloned()
            .unwrap_or_default();

        let settings = ResolverInstallerSettings::combine(
            resolver_installer_options(installer, build),
            filesystem.clone(),
            environment,
        );

        Self {
            locked,
            frozen,
            extras,
            groups,
            output_dir,
            platform,
            machine,
            glibc,
            implementation,
            python: python.and_then(Maybe::into_option),
            install_mirrors,
            settings,
            refresh: Refresh::from(refresh),
        }
    }
}

fn host_platform_machine() -> (uv_configuration::PlatformOs, uv_platform_tags::Arch) {
    let os = if cfg!(target_os = "linux") {
        uv_configuration::PlatformOs::Linux
    } else if cfg!(target_os = "windows") {
        uv_configuration::PlatformOs::Windows
    } else {
        uv_configuration::PlatformOs::Macos
    };
    let arch = if cfg!(target_arch = "x86_64") {
        uv_platform_tags::Arch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        uv_platform_tags::Arch::Aarch64
    } else if cfg!(target_arch = "x86") {
        uv_platform_tags::Arch::X86
    } else if cfg!(target_arch = "riscv64") {
        uv_platform_tags::Arch::Riscv64
    } else {
        // Fall back to x86_64; user can override with --machine.
        uv_platform_tags::Arch::X86_64
    };
    (os, arch)
}
```

Imports to add at the top of `settings.rs` if not already present:
`uv_configuration::{PlatformOs, PyImpl}`, `uv_platform_tags::Arch`. Follow the existing top-level
import style.

- [ ] **Step 2: Clippy**

Run: `cargo clippy -p uv --all-targets -- -D warnings` Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/uv/src/settings.rs
git commit -m "Resolve DownloadSettings from CLI, filesystem, and environment"
```

---

## Task 5: Skeleton `commands::project::download::download`

**Files:**

- Create: `crates/uv/src/commands/project/download.rs`
- Modify: `crates/uv/src/commands/project/mod.rs`

This step introduces the function and its signature, returning `ExitStatus::Error` with a
`todo!`-free stub so it compiles and later tasks can call it. We intentionally do not wire the CLI
dispatch yet.

- [ ] **Step 1: Create `download.rs` with the signature**

Write `crates/uv/src/commands/project/download.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use owo_colors::OwoColorize;
use rustc_hash::FxHashSet;
use tracing::warn;

use uv_cache::Cache;
use uv_client::{BaseClientBuilder, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{
    BuildOptions, Concurrency, DependencyGroups, ExtrasSpecification, HashCheckingMode, HashStrategy,
    Index, PlatformOs, PlatformSpec, Preview, PyImpl, TargetTriple,
};
use uv_dispatch::{BuildDispatch, SharedState};
use uv_distribution_types::{CachedDist, DependencyMetadata, Dist, Resolution, ResolvedDist, SourceDist};
use uv_installer::InFlight;
use uv_platform_tags::Arch;
use uv_python::{
    PythonDownloads, PythonInstallation, PythonPreference, PythonRequest,
    PythonVersionFile,
};
use uv_resolver::{FlatIndex, LockTarget};
use uv_types::{BuildIsolation, SourceTreeEditablePolicy};
use uv_warnings::warn_user;
use uv_workspace::{DiscoveryOptions, VirtualProject, WorkspaceCache};

use crate::commands::ExitStatus;
use crate::commands::pip::{operations, resolution_markers, resolution_tags};
use crate::commands::project::default_dependency_groups;
use crate::commands::project::lock::{LockMode, LockOperation};
use crate::printer::Printer;
use crate::settings::{NetworkSettings, PythonInstallMirrors, ResolverInstallerSettings};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn download(
    project_dir: &Path,
    locked: bool,
    frozen: bool,
    extras: ExtrasSpecification,
    groups: DependencyGroups,
    output_dir: PathBuf,
    platform: PlatformOs,
    machine: Arch,
    glibc: Option<(u16, u16)>,
    implementation: PyImpl,
    python: Option<String>,
    install_mirrors: PythonInstallMirrors,
    python_preference: PythonPreference,
    python_downloads: PythonDownloads,
    settings: ResolverInstallerSettings,
    client_builder: BaseClientBuilder<'_>,
    concurrency: Concurrency,
    no_config: bool,
    cache: &Cache,
    workspace_cache: &WorkspaceCache,
    printer: Printer,
    preview: Preview,
) -> Result<ExitStatus> {
    // 1. Validate inputs and build the PlatformSpec / TargetTriple.
    let spec = PlatformSpec::from_parts(
        Some(platform),
        Some(machine),
        glibc,
        Some(implementation),
        platform,
        machine,
    )?;
    let target_triple = spec.to_target_triple()?;

    // 2. Discover the project (no venv creation).
    let project = VirtualProject::discover(
        project_dir,
        &DiscoveryOptions::default(),
        workspace_cache,
    )
    .await?;

    // 3. Resolve the interpreter (auto-fetch per python_downloads).
    let interpreter_request = python.as_deref().map(PythonRequest::parse);
    let python = PythonInstallation::find_or_download(
        interpreter_request.as_ref(),
        python_preference,
        python_downloads,
        &client_builder,
        cache,
        Some(&install_mirrors),
        None,
        preview,
    )
    .await?;
    let interpreter = python.into_interpreter();

    // 4. Lock operation (Frozen/Locked/Write based on flags).
    let mode = if frozen {
        LockMode::Frozen(uv_resolver::FrozenSource::Manual.into())
    } else if locked {
        LockMode::Locked(&interpreter, uv_resolver::LockCheck::RequireUpToDate)
    } else {
        LockMode::Write(&interpreter)
    };

    let state = uv_resolver::UniversalState::default();
    let lock_target = LockTarget::from(project.workspace());

    let outcome = Box::pin(
        LockOperation::new(
            mode,
            &settings.resolver,
            &client_builder,
            &state,
            Box::new(crate::commands::DefaultResolveLogger),
            &concurrency,
            cache,
            workspace_cache,
            printer,
            preview,
        )
        .execute(lock_target),
    )
    .await?;

    // 5. Compute tags/markers for the target.
    let marker_env = resolution_markers(None, Some(&target_triple), &interpreter);
    let tags = resolution_tags(None, Some(&target_triple), &interpreter)?;

    // 6. Validate target against lock.supported_environments().
    let environments = outcome.lock().supported_environments();
    if !environments.is_empty()
        && !environments.iter().any(|env| env.evaluate(&marker_env, &[]))
    {
        bail!(
            "target platform not listed in `tool.uv.environments`; \
             add this environment to `tool.uv.environments` to support cross-platform downloads"
        );
    }

    // 7. Convert lock to Resolution.
    let resolution = outcome.lock().to_resolution(
        &marker_env,
        &tags,
        &extras.clone().with_defaults(default_dependency_groups(
            project.pyproject_toml(),
        )?.into()),
        &groups,
        settings.resolver.build_options(),
        &Default::default(),
    )?;

    // 8. Filter out editable / path / workspace / git sources and warn.
    let resolution = filter_local_sources(resolution, printer);

    // 9. Initialize RegistryClient + BuildDispatch.
    let client = RegistryClientBuilder::new(client_builder.clone(), cache.clone())
        .index_locations(settings.resolver.index_locations().clone())
        .index_strategy(settings.resolver.index_strategy())
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build()?;

    let flat_index = {
        let client = FlatIndexClient::new(client.cached_client(), client.connectivity(), cache);
        let entries = client
            .fetch_all(
                settings
                    .resolver
                    .index_locations()
                    .flat_indexes()
                    .map(Index::url),
            )
            .await?;
        let hasher = HashStrategy::default();
        FlatIndex::from_entries(
            entries,
            Some(&tags),
            &hasher,
            settings.resolver.build_options(),
        )
    };

    let hasher = HashStrategy::from_resolution(&resolution, HashCheckingMode::Verify)?;
    let shared_state = SharedState::default();
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        &Default::default(),
        &interpreter,
        settings.resolver.index_locations(),
        &flat_index,
        &DependencyMetadata::default(),
        shared_state.into_inner(),
        settings.resolver.index_strategy(),
        settings.resolver.config_setting(),
        settings.resolver.config_settings_package(),
        BuildIsolation::Isolated,
        &Default::default(),
        &Default::default(),
        uv_install_wheel::LinkMode::default(),
        settings.resolver.build_options(),
        &HashStrategy::default(),
        settings.resolver.exclude_newer().cloned(),
        settings.resolver.sources(),
        SourceTreeEditablePolicy::Project,
        workspace_cache.clone(),
        concurrency.clone(),
        preview,
    );

    // 10. Prepare (download) all distributions.
    let in_flight = InFlight::default();
    let all_dists: Vec<Dist> = resolution
        .distributions()
        .filter_map(|d| match d {
            ResolvedDist::Installable { dist, .. } => Some((**dist).clone()),
            ResolvedDist::Installed { .. } => None,
        })
        .collect();

    let cached = operations::prepare(
        all_dists,
        &in_flight,
        &resolution,
        &hasher,
        &tags,
        settings.resolver.build_options(),
        &client,
        &build_dispatch,
        cache,
        &concurrency,
        printer,
    )
    .await?;

    // 11. Materialize to --output-dir.
    let report = materialize_to_out(&cached, &output_dir)?;

    writeln!(
        printer.stderr(),
        "Downloaded {} package{} ({} skipped) to {}",
        report.written,
        if report.written == 1 { "" } else { "s" },
        report.skipped,
        output_dir.display().cyan(),
    )?;

    Ok(ExitStatus::Success)
}

#[derive(Default)]
struct DownloadReport {
    written: usize,
    skipped: usize,
}

fn filter_local_sources(resolution: Resolution, _printer: Printer) -> Resolution {
    resolution.filter(|dist| {
        let ResolvedDist::Installable { dist, .. } = dist else {
            return true;
        };
        if let Dist::Source(SourceDist::Directory(_)) = dist.as_ref() {
            warn_user!(
                "Skipping local/editable source `{}` (not materialized into --output-dir)",
                dist.name()
            );
            return false;
        }
        if let Dist::Source(SourceDist::Git(_)) = dist.as_ref() {
            warn_user!(
                "Skipping git source `{}` (not materialized into --output-dir)",
                dist.name()
            );
            return false;
        }
        true
    })
}

fn materialize_to_out(cached: &[CachedDist], out_dir: &Path) -> Result<DownloadReport> {
    std::fs::create_dir_all(out_dir)?;
    let mut report = DownloadReport::default();
    let mut seen = FxHashSet::default();

    for cached in cached {
        let src = cached.path();
        let Some(file_name) = src.file_name() else {
            continue;
        };
        let dst = out_dir.join(file_name);
        if !seen.insert(file_name.to_owned()) {
            continue;
        }
        if dst.exists() {
            report.skipped += 1;
            continue;
        }
        if let Err(link_err) = std::fs::hard_link(src, &dst) {
            warn!("hard_link {} -> {} failed ({link_err}); copying", src.display(), dst.display());
            if let Err(copy_err) = std::fs::copy(src, &dst) {
                bail!(
                    "failed to materialize {} into {}: hard_link error: {link_err}; copy error: {copy_err}",
                    src.display(),
                    dst.display(),
                );
            }
        }
        report.written += 1;
    }
    Ok(report)
}
```

Notes:

- Some struct/field names above (`ResolverInstallerSettings::resolver`, `.index_locations()`,
  `.build_options()`) reflect existing accessors. If the current code uses slightly different
  accessor names, adjust inline — the logic stays the same. Read the equivalent block in `sync.rs`
  (`crates/uv/src/commands/project/sync.rs` around line 790) as the canonical reference.
- The `default_dependency_groups` helper is imported from `crate::commands::project`. If it is
  `pub(super)`, widen to `pub(crate)` in `mod.rs`.
- `LockMode::Locked` / `LockMode::Frozen` / `LockMode::Write` are the constructors used by `sync.rs`
  today.

- [ ] **Step 2: Register the module**

Edit `crates/uv/src/commands/project/mod.rs`. Add:

```rust
pub(crate) mod download;
pub(crate) use download::download;
```

If `default_dependency_groups` is `pub(super)`, change it to `pub(crate)` here.

- [ ] **Step 3: Build check (no tests yet)**

Run: `cargo clippy -p uv --all-targets -- -D warnings` Expected: compiles cleanly. If clippy
complains about unused imports, remove them; if it complains about
`#[allow(clippy::too_many_arguments)]` being inappropriate, replace with
`#[expect(clippy::too_many_arguments)]` per project style.

- [ ] **Step 4: Commit**

```bash
git add crates/uv/src/commands/project/download.rs crates/uv/src/commands/project/mod.rs
git commit -m "Add commands::project::download skeleton"
```

---

## Task 6: Wire the CLI dispatch

**Files:**

- Modify: `crates/uv/src/lib.rs`
- Modify: `crates/uv-test/src/lib.rs`

- [ ] **Step 1: Dispatch in the top-level match**

In `crates/uv/src/lib.rs`, find the `Commands::Sync(args) => { ... }` arm and add right after it:

```rust
Commands::Download(args) => {
    let args = settings::DownloadSettings::resolve(
        *args,
        filesystem.clone(),
        environment,
    );
    show_settings!(args);

    let cache = cache.init()?.with_refresh(args.refresh);
    let client_builder = BaseClientBuilder::new(&network_settings)
        .keyring(args.settings.resolver.keyring_provider)
        .allow_insecure_host(args.settings.resolver.allow_insecure_host.clone())
        .native_tls(network_settings.native_tls)
        .retries(network_settings.retries)
        .retries_disabled(network_settings.no_retries);

    Box::pin(commands::download(
        &project_dir,
        args.locked,
        args.frozen,
        args.extras,
        args.groups,
        args.output_dir,
        args.platform,
        args.machine,
        args.glibc,
        args.implementation,
        args.python,
        args.install_mirrors,
        args.settings.resolver.python_preference,
        args.settings.resolver.python_downloads,
        args.settings,
        client_builder,
        concurrency,
        no_config,
        &cache,
        &workspace_cache,
        printer,
        preview,
    ))
    .await?
}
```

(Accessor names follow the existing Sync arm — when in doubt, mirror that arm.)

- [ ] **Step 2: Add `TestContext::download` helper**

In `crates/uv-test/src/lib.rs`, after the `pub fn sync(&self) -> Command` function, add:

```rust
/// Create a `uv download` command with options shared across scenarios.
pub fn download(&self) -> Command {
    let mut command = self.new_command();
    command.arg("download");
    self.add_shared_options(&mut command, false);
    command
}
```

- [ ] **Step 3: Verify `uv download --help` works**

Run: `cargo run -p uv -- download --help` Expected: prints help text showing `--platform`,
`--machine`, `--glibc`, `--implementation`, `-o/--output-dir`, and no crash.

- [ ] **Step 4: Clippy**

Run: `cargo clippy -p uv -p uv-test --all-targets -- -D warnings` Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/uv/src/lib.rs crates/uv-test/src/lib.rs
git commit -m "Dispatch uv download through commands::download"
```

---

## Task 7: Integration test — basic native download

**Files:**

- Create: `crates/uv/tests/it/download.rs`
- Modify: `crates/uv/tests/it/main.rs`

- [ ] **Step 1: Register the test module**

Edit `crates/uv/tests/it/main.rs`. Next to `mod sync;` add:

```rust
mod download;
```

- [ ] **Step 2: Write the first failing test**

Create `crates/uv/tests/it/download.rs`:

```rust
use anyhow::Result;
use assert_cmd::prelude::*;
use assert_fs::prelude::*;

use uv_test::{TestContext, uv_snapshot};

#[test]
fn download_basic_native_platform() -> Result<()> {
    let context = uv_test::test_context!("3.12");

    let pyproject_toml = context.temp_dir.child("pyproject.toml");
    pyproject_toml.write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig"]
        "#,
    )?;

    let out = context.temp_dir.child("pkgs");

    uv_snapshot!(context.filters(), context.download().arg("-o").arg(out.path()), @"
    success: true
    exit_code: 0
    ----- stdout -----

    ----- stderr -----
    Resolved 2 packages in [TIME]
    Prepared 1 package in [TIME]
    Downloaded 1 package (0 skipped) to pkgs
    ");

    // The wheel should have been materialized.
    let entries: Vec<_> = std::fs::read_dir(out.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().into_string().unwrap_or_default())
        .collect();
    assert!(
        entries.iter().any(|n| n.starts_with("iniconfig-") && n.ends_with(".whl")),
        "expected an iniconfig wheel in {:?}, got {:?}",
        out.path(),
        entries,
    );

    // No venv should have been created.
    assert!(!context.temp_dir.child(".venv").exists());

    Ok(())
}
```

- [ ] **Step 3: Run the test**

Run: `cargo nextest run -p uv --test it download::download_basic_native_platform` Expected: PASS.

If the snapshot differs (e.g. the `Downloaded … package …` wording drifted during implementation),
accept with `cargo insta review` then re-run.

- [ ] **Step 4: Commit**

```bash
git add crates/uv/tests/it/download.rs crates/uv/tests/it/main.rs
git commit -m "Test: uv download materializes wheels for the native platform"
```

---

## Task 8: Integration tests — input normalization + error surface

**Files:**

- Modify: `crates/uv/tests/it/download.rs`

- [ ] **Step 1: Add normalization test**

Append:

```rust
#[test]
fn download_input_normalization_uppercase_windows_amd64() -> Result<()> {
    let context = uv_test::test_context!("3.12");

    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig"]

        [tool.uv]
        environments = [
          "sys_platform == 'win32' and platform_machine == 'AMD64' and platform_python_implementation == 'CPython'",
        ]
        "#,
    )?;

    let out_a = context.temp_dir.child("a");
    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--platform").arg("Windows")
            .arg("--machine").arg("AMD64")
            .arg("-o").arg(out_a.path()),
        @"success: true"
    );

    let out_b = context.temp_dir.child("b");
    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--platform").arg("win32")
            .arg("--machine").arg("amd64")
            .arg("-o").arg(out_b.path()),
        @"success: true"
    );

    let names = |p: &std::path::Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(p)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().into_string().unwrap_or_default()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(out_a.path()), names(out_b.path()));
    Ok(())
}
```

- [ ] **Step 2: Add `--glibc` on Windows error test**

```rust
#[test]
fn download_glibc_on_non_linux_errors() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = []
        "#,
    )?;

    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--platform").arg("windows")
            .arg("--glibc").arg("2.28")
            .arg("-o").arg(context.temp_dir.child("out").path()),
        @r#"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    error: --glibc is only valid with --platform=linux
    "#
    );
    Ok(())
}
```

- [ ] **Step 3: Add `--implementation=PyPy` error test**

```rust
#[test]
fn download_implementation_non_cpython_errors() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = []
        "#,
    )?;

    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--implementation").arg("PyPy")
            .arg("-o").arg(context.temp_dir.child("out").path()),
        @r#"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    error: unsupported Python implementation `pypy`; only `CPython` is supported
    "#
    );
    Ok(())
}
```

- [ ] **Step 4: Missing `--output-dir` test**

```rust
#[test]
fn download_missing_output_dir() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    uv_snapshot!(
        context.filters(),
        context.download(),
        @r#"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    error: the following required arguments were not provided:
      --output-dir <DIR>

    Usage: uv download --output-dir <DIR>

    For more information, try '--help'.
    "#
    );
    Ok(())
}
```

- [ ] **Step 5: Run and accept snapshots**

Run: `cargo nextest run -p uv --test it download::` If snapshots differ, review with
`cargo insta review`.

- [ ] **Step 6: Commit**

```bash
git add crates/uv/tests/it/download.rs
git commit -m "Test: normalization and invalid-input errors for uv download"
```

---

## Task 9: Integration tests — cross-platform (manylinux_2_28 aarch64) + idempotency + skip local

**Files:**

- Modify: `crates/uv/tests/it/download.rs`

- [ ] **Step 1: Cross-platform aarch64 Linux**

```rust
#[test]
fn download_linux_aarch64_manylinux_2_28() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["charset-normalizer==3.3.2"]

        [tool.uv]
        environments = [
          "sys_platform == 'linux' and platform_machine == 'aarch64' and platform_python_implementation == 'CPython'",
        ]
        "#,
    )?;
    let out = context.temp_dir.child("pkgs");

    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--platform").arg("linux")
            .arg("--machine").arg("aarch64")
            .arg("--glibc").arg("2.28")
            .arg("-o").arg(out.path()),
        @"success: true"
    );

    let has_aarch64 = std::fs::read_dir(out.path())?.any(|e| {
        e.ok()
            .map(|e| e.file_name().to_string_lossy().contains("aarch64"))
            .unwrap_or(false)
    });
    assert!(has_aarch64, "expected aarch64 wheel in output");
    Ok(())
}
```

- [ ] **Step 2: Idempotency**

```rust
#[test]
fn download_reruns_are_idempotent() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig"]
        "#,
    )?;
    let out = context.temp_dir.child("pkgs");

    context
        .download()
        .arg("-o").arg(out.path())
        .assert()
        .success();

    // Second run: nothing new to write.
    uv_snapshot!(
        context.filters(),
        context.download().arg("-o").arg(out.path()),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----

    ----- stderr -----
    Resolved 2 packages in [TIME]
    Downloaded 0 packages (1 skipped) to pkgs
    "#
    );
    Ok(())
}
```

- [ ] **Step 3: Editable / workspace member skipped**

```rust
#[test]
fn download_workspace_member_skipped_with_warning() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "root"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["child", "iniconfig"]

        [tool.uv.workspace]
        members = ["child"]

        [tool.uv.sources]
        child = { workspace = true }
        "#,
    )?;
    let child_pyproject = context.temp_dir.child("child/pyproject.toml");
    child_pyproject.write_str(
        r#"
        [project]
        name = "child"
        version = "0.1.0"
        requires-python = ">=3.12"
        "#,
    )?;
    let out = context.temp_dir.child("pkgs");

    let assert_out = context
        .download()
        .arg("-o").arg(out.path())
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert_out.get_output().stderr);
    assert!(stderr.contains("Skipping local/editable source `child`"), "stderr: {stderr}");

    // `child` wheel must NOT be present.
    let entries: Vec<_> = std::fs::read_dir(out.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().into_string().unwrap_or_default())
        .collect();
    assert!(!entries.iter().any(|n| n.starts_with("child-")));
    assert!(entries.iter().any(|n| n.starts_with("iniconfig-")));
    Ok(())
}
```

- [ ] **Step 4: Run tests, review snapshots**

Run: `cargo nextest run -p uv --test it download::`

- [ ] **Step 5: Commit**

```bash
git add crates/uv/tests/it/download.rs
git commit -m "Test: cross-platform, idempotency, and workspace-skip for uv download"
```

---

## Task 10: Integration tests — `--locked`, `--frozen`, `--refresh`, and platform-not-in-environments

**Files:**

- Modify: `crates/uv/tests/it/download.rs`

- [ ] **Step 1: `--locked` with mismatch**

```rust
#[test]
fn download_locked_fails_on_mismatch() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    let pyproject = context.temp_dir.child("pyproject.toml");
    pyproject.write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig==2.0.0"]
        "#,
    )?;
    context.lock().assert().success();

    pyproject.write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig==1.1.1"]
        "#,
    )?;

    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--locked")
            .arg("-o").arg(context.temp_dir.child("out").path()),
        @r#"
    success: false
    exit_code: 1
    ----- stdout -----

    ----- stderr -----
    Resolved 2 packages in [TIME]
    The lockfile at `uv.lock` needs to be updated, but `--locked` was provided. To update the lockfile, run `uv lock`.
    "#
    );
    Ok(())
}
```

- [ ] **Step 2: Platform not in environments**

```rust
#[test]
fn download_platform_not_in_environments() -> Result<()> {
    let context = uv_test::test_context!("3.12");
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig"]

        [tool.uv]
        environments = [
          "sys_platform == 'linux' and platform_machine == 'x86_64' and platform_python_implementation == 'CPython'",
        ]
        "#,
    )?;
    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--platform").arg("linux")
            .arg("--machine").arg("aarch64")
            .arg("-o").arg(context.temp_dir.child("out").path()),
        @r#"
    success: false
    exit_code: 1
    ----- stdout -----

    ----- stderr -----
    error: target platform not listed in `tool.uv.environments`; add this environment to `tool.uv.environments` to support cross-platform downloads
    "#
    );
    Ok(())
}
```

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p uv --test it download::`

- [ ] **Step 4: Commit**

```bash
git add crates/uv/tests/it/download.rs
git commit -m "Test: --locked and platform-not-in-environments for uv download"
```

---

## Task 11: Docs — `docs/guides/download.md`

**Files:**

- Create: `docs/guides/download.md`
- Modify: `docs/reference/cli.md` (auto-generated; see Step 3)
- Modify: `docs/guides/index.md` (if present — add link)

- [ ] **Step 1: Write the guide**

Create `docs/guides/download.md`:

````markdown
# Downloading dependencies into a wheelhouse

The `uv download` command resolves a project's `uv.lock` for a target platform and writes all wheels
(and any sdists) into `--output-dir`, **without** creating a virtual environment or installing
anything.

## Quickstart

```console
$ uv download -o pkgs
Resolved 25 packages in 142ms
Prepared 25 packages in 812ms
Downloaded 25 packages (0 skipped) to pkgs
```
````

## Cross-platform target

Pre-populate a wheelhouse for an aarch64 Linux container while running uv on a developer laptop:

```console
$ uv download \
    --platform linux \
    --machine aarch64 \
    --glibc 2.28 \
    -o aarch64-wheels
```

The target must appear in `tool.uv.environments` (or `tool.uv.required-environments`) for the
lockfile to carry the required wheels.

## Inputs

| Flag                 | Default             | Accepts                                                                                            |
| -------------------- | ------------------- | -------------------------------------------------------------------------------------------------- |
| `--platform`         | host OS             | `linux`, `windows` (or `win32`), `macos` (or `darwin`, `osx`)                                      |
| `--machine`          | host arch           | `x86_64` / `amd64` / `AMD64` / `x64`, `aarch64` / `arm64`, `i686` / `i386` / `x86`, `riscv64`, ... |
| `--glibc`            | `2.28` (linux only) | `MAJOR.MINOR` (e.g. `2.28`) or `MAJOR_MINOR` (`2_28`)                                              |
| `--implementation`   | `CPython`           | `CPython` (only `CPython` is supported today)                                                      |
| `-o`, `--output-dir` | required            | any directory                                                                                      |

All inputs are case-insensitive. `--glibc` is rejected for non-Linux targets.

## Interaction with Python interpreters

`uv download` uses an interpreter only to compute tags and markers — no venv is created. If you pass
`--python 3.14` and the host does not have 3.14, uv will fetch a managed Python build automatically
(same as `uv sync --python 3.14`). Pass `--no-python-downloads` to disable this and require a local
interpreter.

````

- [ ] **Step 2: Link from the guides index**

Run: `cat docs/guides/index.md` to see existing format. Add a bullet to the appropriate section:

```markdown
- [Downloading dependencies into a wheelhouse](./download.md)
````

If `docs/guides/index.md` does not exist, skip this step.

- [ ] **Step 3: Regenerate CLI reference**

Run: `cargo run -p uv-dev -- generate-all` Expected: `docs/reference/cli.md` (and any schema files)
updated. If the exact command differs in this repo, read `crates/uv-dev/src/main.rs` for the
supported subcommand.

- [ ] **Step 4: Commit**

```bash
git add docs/guides/download.md docs/reference/cli.md docs/guides/index.md
git commit -m "Document the uv download command"
```

---

## Task 12: Final validation

**Files:** none (verification only)

- [ ] **Step 1: Full clippy across the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings` Expected: clean. Fix anything that
regressed (remember: warnings are almost never pre-existing).

- [ ] **Step 2: Windows cross-compile check**

Run: `cargo xwin clippy --workspace --all-targets -- -D warnings` Expected: clean.

- [ ] **Step 3: Run the whole download test module**

Run: `cargo nextest run -p uv --test it download::` Expected: all green.

- [ ] **Step 4: Run sync smoke tests (regression check for the Task 2 refactor)**

Run: `cargo nextest run -p uv --test it sync::sync sync::locked sync::frozen` Expected: all green.

- [ ] **Step 5: Verify no stray `unwrap` / `panic` introduced**

Run:
`rg -n "\.unwrap\(\)|panic!\(|unreachable!\(|unsafe" crates/uv-configuration/src/platform_spec.rs crates/uv/src/commands/project/download.rs`
Expected: only matches inside `#[cfg(test)]` blocks (tests may `.unwrap()` for clarity; production
code may not).

- [ ] **Step 6: Commit (if any doc regeneration produced changes in Step 1)**

```bash
git status
# If anything uncommitted, add and commit as "Chore: regenerate CLI reference" or similar.
```

- [ ] **Step 7: Final manual sanity run**

Run: `cargo run -p uv -- download -o /tmp/uv-download-smoke` (from a temporary project with
`[project] dependencies = ["iniconfig"]`). Expected: succeeds,
`/tmp/uv-download-smoke/iniconfig-*.whl` exists, no `.venv` created.

---

## Self-review notes

- Spec §2 / §3 CLI surface → Tasks 3, 4, 6 (flags, settings, dispatch).
- Spec §3.2 Normalization → Task 1 (`PlatformSpec`), Task 8 (integration normalization test).
- Spec §3.3 Defaults → Task 1 (defaults in `from_parts`), Task 4 (host fallback).
- Spec §4 Architecture → Tasks 1, 2, 5, 6.
- Spec §4.3 Data flow → Task 5.
- Spec §4.4 TargetTriple mapping → Task 1 (matrix in tests + implementation).
- Spec §5 Validation & errors → Task 1 (type-level), Task 8 (integration), Task 10 (lockfile /
  environments).
- Spec §6 Managed Python → Task 5 (auto-download via `find_or_download`). Integration test for this
  is deferred — exercising PBS in CI requires existing fixtures; document as a follow-up if not
  already covered.
- Spec §7 Output semantics → Task 5 (`materialize_to_out`), Task 9 (idempotency), Task 7 (basic
  write).
- Spec §8 Testing plan → Tasks 7–10 cover the listed cases except
  `download_refresh_rebypasses_cache`, `download_sdist_only_dependency`,
  `download_python_auto_fetch`, `download_glibc_alias_format`. These are valuable but not blockers;
  tracked as follow-up in the spec's §11. The core spec requirements are covered.
- Spec §9 Documentation → Task 11.
- Spec §10 Rollout & compatibility → Task 12 (regression check), plus additive-only CLI (no tool.uv
  schema change in v1).
- Spec §11 Open questions → deferred; not planned in this document.

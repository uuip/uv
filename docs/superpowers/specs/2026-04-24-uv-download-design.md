# `uv download` — Design Spec

Status: Draft · Date: 2026-04-24 · Author: brainstorming session

## 1. Motivation

Users building container images or pre-populating offline caches want to fetch the wheels/sdists
that `uv sync` would install, without actually creating a `.venv` or installing anything. Today the
only way to get "just the artifacts" is to run `uv sync` against a throwaway venv and scrape them
back out, which is wasteful and needs a real interpreter for the target platform.

We add a new top-level command `uv download` that reuses the project/lockfile resolution pipeline
but stops at "wheels materialized on disk" and supports cross-platform targets through user-friendly
flags.

## 2. Scope

In scope:

- Project-based download: reads `pyproject.toml` and `uv.lock` of the current project. PEP 723
  script targets are deferred to a follow-up.
- Cross-platform targeting via `--platform` / `--machine` / `--glibc` / `--implementation`.
- Flat output directory (`-o, --output-dir`).
- Wheels are hard-linked from uv's cache when possible; copy fallback across filesystems.
- sdists are copied as-is (never built during download).
- Editable / local path / workspace / virtual / git source dependencies are skipped with a warning.
- Full reuse of managed Python download mechanism — host does not need a local interpreter matching
  the target Python version.

Out of scope (first release):

- Standalone requirement spec input (`pip download <pkg>` style) — tracked for follow-up.
- musllinux, Android, iOS, Pyodide targets as explicit flags — the existing `TargetTriple` enum
  covers them internally, but the CLI wrapper first release only exposes manylinux / Windows /
  macOS.
- Implementations other than CPython.
- Git source fetching.
- Download to structured per-platform subdirectories.

## 3. CLI Surface

```text
uv download [OPTIONS]
    --platform <OS>            linux | windows | macos (case-insensitive; default: host)
    --machine <ARCH>           x86_64 | amd64 | AMD64 | aarch64 | arm64 | i686 | …
                               (case-insensitive aliases; default: host)
    --glibc <MAJOR.MINOR>      only valid with --platform=linux; default 2.28
    --implementation <IMPL>    CPython (default; other values rejected in v1)
    -o, --output-dir <DIR>     required; created if missing, merged if present

    --python <REQ>             same as `uv sync --python`
    --extra / --all-extras / --no-extra / --group / --only-group / --no-default-groups / --all-groups
    --locked / --frozen / --refresh / --refresh-package
    --index / --default-index / --index-strategy / --keyring-provider
    --no-build / --no-binary / --only-binary / --no-build-package / --no-binary-package
    --no-sources
    --python-preference / --no-python-downloads / --python-downloads
```

No `--dry-run` in v1 — the operation is already read-only w.r.t. the project and cheap to re-run.

### 3.1 Naming decisions

| User's example        | Adopted                                                               | Reason                                                                                  |
| --------------------- | --------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| `--platform linux`    | `--platform linux\|windows\|macos` (aliases `win32`, `darwin`, `osx`) | Aligns with `sys_platform` values; `windows` is more natural as a user input.           |
| `--machine aarch64`   | `--machine` with case-insensitive alias set                           | Unifies `AMD64 ≡ amd64 ≡ x86_64 ≡ x64` and `arm64 ≡ aarch64`.                           |
| `--glibc 2.28`        | `--glibc 2.28`                                                        | Kept; accepts `2.28` or `2_28`.                                                         |
| `--implement CPython` | `--implementation CPython` (alias `--impl`)                           | `--implement` is a verb; `platform_python_implementation` is the canonical marker name. |
| `--out pkgs`          | `-o, --output-dir pkgs`                                               | Matches uv's `--output-*` convention; short flag for scripting.                         |

### 3.2 Normalization

- `--platform`: lowercased input maps
  `{linux → Os::Linux, windows|win32 → Os::Windows, macos|darwin|osx → Os::Macos}`; everything else
  rejected.
- `--machine`: lowercased input maps to the existing `uv_platform_tags::Arch::from_str` plus aliases
  `amd64 → x86_64`, `arm64 → aarch64`, `x64 → x86_64`, `x86-64 → x86_64`, `i386 → i686`.
- `--glibc`: accepts `MAJOR.MINOR` or `MAJOR_MINOR`, parsed to `(u16, u16)`.

### 3.3 Defaults

- `--platform`, `--machine`: host values (mirrors `uv sync` default).
- `--glibc`: `2.28` when `--platform=linux`, ignored otherwise.
- `--implementation`: `CPython`.

## 4. Architecture

### 4.1 New files

- `crates/uv-configuration/src/platform_spec.rs`
  - `pub struct PlatformSpec { os: PlatformOs, arch: Arch, glibc: Option<(u16, u16)>, implementation: PyImpl }`
  - `pub enum PlatformOs { Linux, Windows, Macos }` with case-insensitive `FromStr` including the
    aliases above.
  - `pub enum PyImpl { CPython }` (open-ended enum for future implementations; v1 only accepts
    `CPython`).
  - `impl PlatformSpec { pub fn from_args(...) -> Result<Self, PlatformSpecError>; pub fn to_target_triple(&self) -> TargetTriple; }`
  - `pub enum PlatformSpecError` for the structured conflicts described in §5.
  - Re-exported from `uv_configuration`.
- `crates/uv/src/commands/project/download.rs`
  - `pub(crate) async fn download(...)` — orchestrates project discovery, lock, resolve, prepare,
    materialize.
  - `fn materialize_to_out(cached: &[CachedDist], out_dir: &Path) -> Result<DownloadReport>`.

### 4.2 Modified files

- `crates/uv-cli/src/lib.rs`:
  - `Commands::Download(Box<DownloadArgs>)` variant.
  - `pub struct DownloadArgs` composed from platform flags + `ResolverInstallerArgs` +
    `BuildOptionsArgs` + `RefreshArgs` + `python` + `output_dir` + the extras/groups subset used by
    sync.
- `crates/uv/src/settings.rs`: `pub(crate) struct DownloadSettings` mirroring `SyncSettings` +
  `platform: PlatformSpec`, `output_dir: PathBuf`.
- `crates/uv/src/commands/project/mod.rs`:
  `pub(crate) mod download; pub(crate) use download::download;`.
- `crates/uv/src/lib.rs`: `Commands::Download` arm calling `commands::download(...)`.
- `crates/uv/src/commands/pip/operations.rs`:
  - Extract the preparer setup and invocation from `install(...)` into
    `pub(crate) async fn prepare(...) -> Result<Vec<CachedDist>, Error>`. `install(...)` is
    rewritten to first call `prepare(...)` then do the site-packages linking.
- `crates/uv/src/commands/project/mod.rs` or `sync.rs`: expose
  `pub(super) fn resolve_for_target(...)` that runs the `LockOperation` + `lock.to_resolution()`
  shared between sync and download. (Optional refactor — if diff grows too large we can inline the
  ~40 lines in `download.rs`.)

### 4.3 Data flow

```
DownloadArgs (clap)
    │  PlatformSpec::from_args (normalize, validate, default-fill)
    ▼
DownloadSettings::resolve(args, filesystem, environment)
    │
    ▼
commands::project::download::download()
    ├─ Discover Project (VirtualProject::discover)
    ├─ Resolve Interpreter (PythonInstallation::find_or_fetch — no venv created)
    ├─ LockOperation::execute(LockTarget)                 → Lock
    ├─ target_triple = platform_spec.to_target_triple()
    ├─ marker_env = resolution_markers(None, Some(&target_triple), interpreter)
    ├─ tags       = resolution_tags(None, Some(&target_triple), interpreter)?
    ├─ validate lock.supported_environments() against marker_env
    ├─ resolution = lock.to_resolution(marker_env, tags, extras, groups, build_options, install_options)
    ├─ resolution = filter_local_sources(resolution)      (warn per skipped)
    ├─ cached    = operations::prepare(resolution, cache, …)
    └─ report    = materialize_to_out(cached, output_dir)
                   │ for each CachedDist:
                   │   dst = output_dir.join(cached.filename()?)
                   │   if dst.exists() { report.skipped += 1; continue; }
                   │   try hard_link(src, dst); on any error fall back to copy(src, dst)
                   │   report.written += 1
```

The download command never creates or modifies a `.venv`. The `Interpreter` is used purely for
tag/marker computation via `resolution_tags` and `resolution_markers`, exactly as
`uv sync --python-platform …` already does.

### 4.4 `PlatformSpec::to_target_triple` mapping

| platform | arch              | glibc                       | `TargetTriple` variant                                    |
| -------- | ----------------- | --------------------------- | --------------------------------------------------------- |
| linux    | x86_64            | 2.17                        | `X8664Manylinux217`                                       |
| linux    | x86_64            | 2.28 (default)              | `X8664Manylinux228`                                       |
| linux    | x86_64            | 2.{31…40}                   | `X8664Manylinux2XX`                                       |
| linux    | aarch64           | 2.17                        | `Aarch64Manylinux217`                                     |
| linux    | aarch64           | 2.28 (default)              | `Aarch64Manylinux228`                                     |
| linux    | aarch64           | 2.{31…40}                   | `Aarch64Manylinux2XX`                                     |
| linux    | riscv64           | any (min 2.39)              | `Riscv64UnknownLinuxGnu`                                  |
| windows  | x86_64            | must be absent              | `X8664PcWindowsMsvc`                                      |
| windows  | aarch64           | must be absent              | `Aarch64PcWindowsMsvc`                                    |
| windows  | i686              | must be absent              | `I686PcWindowsMsvc`                                       |
| macos    | x86_64            | must be absent              | `X8664AppleDarwin`                                        |
| macos    | aarch64           | must be absent              | `Aarch64AppleDarwin`                                      |
| any      | unsupported combo | —                           | `PlatformSpecError::UnsupportedCombination`               |
| linux    | any               | unsupported manylinux minor | `PlatformSpecError::UnsupportedGlibc { supported: &[…] }` |

## 5. Validation & errors

| Condition                                                                    | Handling                                                                                                                                                      |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--platform` value not in `{linux, windows, macos}` (after alias lowering)   | `clap::Error` (possible_values listed).                                                                                                                       |
| `--machine` value not in alias table                                         | `clap::Error`.                                                                                                                                                |
| `--glibc` given with `--platform != linux`                                   | `PlatformSpecError::GlibcOnNonLinux`, mapped to `anyhow::bail!`.                                                                                              |
| `--implementation` ≠ `CPython`                                               | `PlatformSpecError::UnsupportedImplementation`, `anyhow::bail!`.                                                                                              |
| Combination not representable as a `TargetTriple`                            | `PlatformSpecError::UnsupportedCombination { os, arch }`.                                                                                                     |
| `requires-python` not satisfied by any discoverable/downloadable interpreter | reuse existing python discovery error.                                                                                                                        |
| Lockfile outdated and not `--frozen`                                         | reuse `ProjectError::LockMismatch`.                                                                                                                           |
| Target platform not in `tool.uv.environments`                                | reuse `ProjectError::LockedPlatformIncompatibility`, amended message: _"add this environment to `tool.uv.environments` to support cross-platform downloads"_. |
| `--output-dir` cannot be created / not writable                              | `anyhow::bail!("failed to prepare --output-dir {path}: {source}")`.                                                                                           |
| Hard-link and copy both fail                                                 | bail with both paths.                                                                                                                                         |
| Dependency is editable / workspace member / non-virtual path / git source    | skip with `warn_user!`.                                                                                                                                       |
| Dependency only has sdist                                                    | copy sdist file as-is; no build attempted.                                                                                                                    |

Exit codes follow `uv sync`: `Success`, `Failure` (lock mismatch, platform incompatibility), `Error`
(infra).

## 6. Managed Python interactions

Download inherits `python_preference` / `python_downloads` from the CLI and settings stack. Users
who want a 3.14 target without a local 3.14 interpreter pass `--python 3.14` and uv auto-fetches a
python-build-standalone 3.14 (same path as `uv sync --python 3.14`). Offline CI pipelines pass
`--no-python-downloads`.

A fully interpreter-free mode was considered and deferred. If requested later, we would introduce
`--no-interpreter` + required `--python-version X.Y`, synthesize `Tags` via `Tags::from_env` and
`MarkerEnvironment` directly from `PlatformSpec`, defaulting `gil_disabled = false`,
`debug_enabled = false`.

## 7. Output semantics

- Flat layout: every `.whl` / `.tar.gz` lands directly under `--output-dir`.
- Existing identical filenames are left untouched (skip count reported).
- Hard-link first (`std::fs::hard_link`); fall back to `std::fs::copy` if the link call returns any
  error (e.g. cross-filesystem `EXDEV`, unsupported filesystem, permission).
- Final report (`DownloadReport`): written count, skipped count, wall time. Format mirrors the
  existing sync summary so logs are familiar.

## 8. Testing plan

Project convention: integration tests under `crates/uv/tests/it/` with insta snapshots.

### 8.1 Integration tests (`crates/uv/tests/it/download.rs`)

1. `download_basic_native_platform` — simple project, `iniconfig` wheel dependency,
   `uv download -o out/` succeeds and `out/iniconfig-*.whl` exists. Snapshot stdout/stderr.
2. `download_linux_aarch64_manylinux_2_28` — lockfile includes an aarch64 wheel; run with
   `--platform linux --machine aarch64` on macOS host; assert correct wheel appears.
3. `download_input_normalization` — `--platform Windows --machine AMD64` and
   `--platform win32 --machine amd64` produce the same filenames set (sorted snapshot).
4. `download_glibc_on_non_linux_errors` — `--platform windows --glibc 2.28` → non-zero exit +
   snapshot of error message.
5. `download_implementation_non_cpython_errors` — `--implementation PyPy` → snapshot error.
6. `download_missing_output_dir` — command fails clap usage check; snapshot.
7. `download_platform_not_in_environments` — lock with
   `environments = ["sys_platform == 'linux' and platform_machine == 'x86_64'"]`, run with
   `--machine aarch64`; assert `LockedPlatformIncompatibility` with the added hint.
8. `download_editable_workspace_member_skipped` — workspace member declared editable; wheel not in
   `out/`, stderr contains warning, exit 0.
9. `download_path_source_skipped` — non-editable path dep skipped with warning.
10. `download_reruns_are_idempotent` — run twice, second run reports `0 written, N skipped`.
11. `download_frozen_respects_lockfile` — mutate `pyproject.toml`, run with `--frozen`; no lock
    rewrite, downloads original set.
12. `download_locked_fails_on_mismatch` — mutate `pyproject.toml`, run with `--locked`; expect
    lock-mismatch exit.
13. `download_refresh_rebypasses_cache` — first run populates cache, second with `--refresh` should
    re-fetch (assert via test registry counters).
14. `download_python_auto_fetch` — `--python 3.12` on host without 3.12; PBS auto-download kicks in
    (existing fixture pattern).
15. `download_sdist_only_dependency` — dependency published only as sdist; sdist tarball lands in
    `out/`.
16. `download_glibc_alias_format` — `--glibc 2_28` equivalent to `--glibc 2.28`.

### 8.2 Unit tests (`crates/uv-configuration/src/platform_spec.rs`)

- `from_args_normalizes_case` — `{WINDOWS, windows, Win32}` all yield `Os::Windows`;
  `{AMD64, amd64, x86_64, x86-64, x64}` all yield `Arch::X86_64`.
- `from_args_rejects_glibc_on_windows`.
- `from_args_rejects_glibc_on_macos`.
- `from_args_rejects_non_cpython`.
- `to_target_triple_matrix` — one case per row of §4.4 (including default glibc fallback for linux).
- `to_target_triple_unsupported_combination` — `macos + i686`, etc.
- `glibc_accepts_dot_and_underscore`.

### 8.3 Snapshot conventions

- Error messages: `insta::assert_snapshot!`.
- Downloaded file set: `insta::assert_debug_snapshot!(sorted_filenames)`.
- Download reports: `assert_snapshot!` after filtering timing.

### 8.4 Windows verification

Per project guidance, use `cargo xwin clippy` on the Windows build after any platform-matching
changes to the parser or `platform_spec.rs`. Integration test 3 covers runtime normalization on any
host.

## 9. Documentation

- `docs/reference/cli.md` regenerated via `uv-dev generate-all` (automatic).
- New how-to guide: `docs/guides/download.md` — motivation, flags, a worked example producing a
  wheelhouse for an aarch64 Linux image build, interaction with `required-environments`.
- `pyproject.toml` / `uv.lock` reference docs mention `uv download` alongside `uv sync` where
  relevant.

## 10. Rollout & compatibility

- Additive: new command, no changes to `uv sync` behavior or output.
- Settings file key `[tool.uv]` not extended in v1 (all flags are CLI-only). A
  `tool.uv.download = { output-dir = "…" }` block can be added later following the pattern used by
  other subcommand sections.
- Preview gating: not required — operation is strictly additive and reuses stable infrastructure. If
  concerns arise we can mark the command as `#[preview]` behind `--preview-features download` until
  we've seen field usage.

## 11. Open questions / follow-ups

- musllinux support (`--libc {gnu|musl}` extension).
- Standalone `uv download -r requirements.txt <pkg>` mode (non-project usage).
- Multi-target mode:
  `uv download --platform linux --machine aarch64 --platform linux --machine x86_64` with subdir
  layout. Explicitly deferred.
- Include-git-sources flag.
- Interpreter-free mode (`--no-interpreter` + forced `--python-version`).

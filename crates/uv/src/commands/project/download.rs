use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use futures::StreamExt;
use owo_colors::OwoColorize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::warn;
use uuid::Uuid;

use uv_cache::Cache;
use uv_client::{BaseClientBuilder, RegistryClient, RegistryClientBuilder};
use uv_configuration::{
    Concurrency, DependencyGroups, DependencyGroupsWithDefaults, ExtrasSpecification,
    InstallOptions, PlatformOs, PlatformSpec, PyImpl, TargetTriple,
};
use uv_distribution_types::{BuiltDist, Dist, IndexUrl, RemoteSource, ResolvedDist, SourceDist};
use uv_extract::hash::Hasher;
use uv_normalize::DefaultExtras;
use uv_platform_tags::Arch;
use uv_preview::Preview;
use uv_pypi_types::HashDigest;
use uv_python::{Interpreter, PythonDownloads, PythonPreference, PythonRequest};
use uv_redacted::DisplaySafeUrl;
use uv_resolver::{Installable, Lock};
use uv_settings::PythonInstallMirrors;
use uv_warnings::warn_user;
use uv_workspace::{DiscoveryOptions, MemberDiscovery, VirtualProject, WorkspaceCache};

use crate::commands::pip::loggers::DefaultResolveLogger;
use crate::commands::pip::{resolution_markers, resolution_tags};
use crate::commands::project::install_target::InstallTarget;
use crate::commands::project::lock::{LockMode, LockOperation};
use crate::commands::project::lock_target::LockTarget;
use crate::commands::project::{
    ProjectError, ProjectInterpreter, UniversalState, WorkspacePython, default_dependency_groups,
};
use crate::commands::reporters::DownloadProjectReporter;
use crate::commands::{ExitStatus, diagnostics};
use crate::printer::Printer;
use crate::settings::{FrozenSource, LockCheck, ResolverInstallerSettings};

/// Download the project's pinned dependencies as wheel or sdist files into `output_dir`.
///
/// Resolves (or reads) the lockfile for the requested target platform, then directly
/// downloads every artifact to `output_dir` without extracting, building, or re-archiving.
/// The output files are byte-identical to what was published on the index; their SHA-256
/// matches the hashes in `uv.lock`.
///
/// No virtual environment is created or modified.
#[expect(clippy::too_many_arguments)]
pub(crate) async fn download(
    project_dir: &Path,
    lock_check: LockCheck,
    frozen: Option<FrozenSource>,
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
    // 1. Build PlatformSpec -> TargetTriple for the requested target platform.
    let spec = PlatformSpec::from_parts(
        Some(platform),
        Some(machine),
        glibc,
        Some(implementation),
        platform,
        machine,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let target_triple: TargetTriple = spec.to_target_triple().map_err(|e| anyhow::anyhow!(e))?;

    // 2. Discover the project workspace (no venv creation).
    let project = if frozen.is_some() {
        VirtualProject::discover(
            project_dir,
            &DiscoveryOptions {
                members: MemberDiscovery::None,
                ..DiscoveryOptions::default()
            },
            workspace_cache,
        )
        .await?
    } else {
        VirtualProject::discover(project_dir, &DiscoveryOptions::default(), workspace_cache).await?
    };

    // Compute the default dependency groups and extras for the workspace.
    let default_groups = default_dependency_groups(project.pyproject_toml())?;
    let groups = groups.with_defaults(default_groups);
    let extras = extras.with_defaults(DefaultExtras::default());

    // Initialize shared state for locking.
    let state = UniversalState::default();

    // 3. Resolve the interpreter. Declared here so it outlives `mode`.
    //    When frozen, we skip this now and do it after the lock step.
    let maybe_interpreter: Option<Interpreter> = if frozen.is_some() {
        None
    } else {
        let groups_for_discovery = DependencyGroupsWithDefaults::none();
        let workspace_python = WorkspacePython::from_request(
            python.as_deref().map(PythonRequest::parse),
            Some(project.workspace()),
            &groups_for_discovery,
            project_dir,
            no_config,
        )
        .await?;
        Some(
            ProjectInterpreter::discover(
                project.workspace(),
                &groups_for_discovery,
                workspace_python,
                &client_builder,
                python_preference,
                python_downloads,
                &install_mirrors,
                false,
                // `Some(false)` prevents ProjectInterpreter from creating a venv.
                Some(false),
                cache,
                printer,
                preview,
            )
            .await?
            .into_interpreter(),
        )
    };

    // 4. Determine lock mode from `--locked` / `--frozen` / default write.
    let mode = if let Some(frozen_source) = frozen {
        LockMode::Frozen(frozen_source.into())
    } else {
        let Some(interpreter) = maybe_interpreter.as_ref() else {
            bail!("internal error: interpreter should be resolved when not frozen");
        };
        if let LockCheck::Enabled(lock_check_source) = lock_check {
            LockMode::Locked(interpreter, lock_check_source)
        } else {
            LockMode::Write(interpreter)
        }
    };

    // 5. Execute the lock operation (resolve / read the lockfile).
    let lock_target = LockTarget::from(project.workspace());

    let outcome = match Box::pin(
        LockOperation::new(
            mode,
            &settings.resolver,
            &client_builder,
            &state,
            Box::new(DefaultResolveLogger),
            &concurrency,
            cache,
            workspace_cache,
            printer,
            preview,
        )
        .execute(lock_target),
    )
    .await
    {
        Ok(result) => result,
        Err(ProjectError::Operation(err)) => {
            return diagnostics::OperationDiagnostic::with_system_certs(
                client_builder.system_certs(),
            )
            .report(err)
            .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
        }
        Err(ProjectError::LockMismatch(prev, cur, lock_source)) => {
            writeln!(
                printer.stderr(),
                "{}",
                ProjectError::LockMismatch(prev, cur, lock_source)
                    .to_string()
                    .bold()
            )?;
            return Ok(ExitStatus::Failure);
        }
        Err(err) => return Err(err.into()),
    };

    let lock = outcome.lock();

    // When frozen, discover the interpreter now (needed for marker/tag evaluation).
    let frozen_interpreter: Option<Interpreter> = if frozen.is_some() {
        let groups_for_discovery = DependencyGroupsWithDefaults::none();
        let workspace_python = WorkspacePython::from_request(
            python.as_deref().map(PythonRequest::parse),
            Some(project.workspace()),
            &groups_for_discovery,
            project_dir,
            no_config,
        )
        .await?;
        Some(
            ProjectInterpreter::discover(
                project.workspace(),
                &groups_for_discovery,
                workspace_python,
                &client_builder,
                python_preference,
                python_downloads,
                &install_mirrors,
                false,
                Some(false),
                cache,
                printer,
                preview,
            )
            .await?
            .into_interpreter(),
        )
    } else {
        None
    };

    let Some(interpreter) = frozen_interpreter.as_ref().or(maybe_interpreter.as_ref()) else {
        bail!("internal error: interpreter should be resolved at this point");
    };

    // 6. Compute marker environment and tags for the target platform.
    let marker_env = resolution_markers(None, Some(&target_triple), interpreter);
    let tags = resolution_tags(None, Some(&target_triple), interpreter)?;

    // 7. Validate the target platform against the lock's supported environments.
    let environments = lock.supported_environments();
    if !environments.is_empty()
        && !environments.iter().any(|env| env.evaluate(&marker_env, &[]))
    {
        bail!(
            "target platform not listed in `tool.uv.environments`; \
             add this environment to `tool.uv.environments` to support cross-platform downloads"
        );
    }

    // Build an InstallTarget covering the full workspace so all packages are included.
    let install_target = make_install_target(&project, lock);

    // 8. Convert the lock to a Resolution for the target platform.
    let install_options = InstallOptions::default();
    let resolution = install_target.to_resolution(
        &marker_env,
        &tags,
        &extras,
        &groups,
        &settings.resolver.build_options,
        &install_options,
    )?;

    // 9. Build RegistryClient for direct-URL downloads.
    let index_locations = &settings.resolver.index_locations;
    let index_strategy = settings.resolver.index_strategy;
    let keyring_provider = settings.resolver.keyring_provider;
    let client_builder = client_builder.clone().keyring(keyring_provider);

    // If the user explicitly pointed `--default-index` at a non-PyPI mirror, rewrite the
    // Registry artifact URLs stored in the lockfile to point at that mirror. The lockfile
    // bakes in whatever URL the original resolve saw (typically `files.pythonhosted.org`
    // for PyPI), so without this, passing `--default-index` to `uv download` would have
    // no effect on a pre-existing lock. The mirror base is derived by stripping the
    // trailing `simple` segment from the index URL, which matches the layout published by
    // bandersnatch-style mirrors (Tsinghua, USTC, Aliyun). Warn rather than silently
    // ignore the flag when the shape is one we don't know how to turn into a file base —
    // a silent no-op here is confusing, especially for local-path indexes.
    let mirror_base = match index_locations.default_index() {
        Some(index) if matches!(index.url, IndexUrl::Pypi(_)) => None,
        Some(index) => match (&index.url, index.url.root()) {
            (IndexUrl::Path(_), _) => {
                warn_user!(
                    "`--default-index` points at a local path; `uv download` cannot rewrite \
                     recorded artifact URLs to a filesystem index and will use the URLs in \
                     `uv.lock` as-is"
                );
                None
            }
            (_, None) => {
                warn_user!(
                    "`--default-index` was provided but its URL does not end in `simple` / \
                     `+simple`; `uv download` does not know how to derive a mirror file \
                     base and will use the URLs in `uv.lock` as-is"
                );
                None
            }
            (_, Some(root)) => Some(root),
        },
        None => None,
    };

    let client = RegistryClientBuilder::new(client_builder.clone(), cache.clone())
        .index_locations(index_locations.clone())
        .index_strategy(index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build()?;
    let client = Arc::new(client);

    // 10. Ensure the output directory exists.
    fs_err::create_dir_all(&output_dir)?;

    // 11. Walk the resolution and spawn per-artifact download tasks onto a JoinSet.
    //     Downloads run in parallel, gated by `concurrency.downloads_semaphore` to mirror
    //     the rate-limiting behaviour of `uv sync`'s preparer. Local copy/link and skip
    //     arms run inline because they don't issue network requests.
    //
    //     The reporter length counts only artifacts that actually hit the network so the
    //     top-level `(pos/len)` matches the spawned task count — local hard-link/copy
    //     entries and skipped variants are not tallied.
    let remote_count = resolution
        .hashes()
        .filter(|(resolved, _)| {
            let ResolvedDist::Installable { dist, .. } = resolved else {
                return false;
            };
            matches!(
                dist.as_ref(),
                Dist::Built(BuiltDist::Registry(_) | BuiltDist::DirectUrl(_))
                    | Dist::Source(SourceDist::Registry(_) | SourceDist::DirectUrl(_))
            )
        })
        .count() as u64;
    let reporter = Arc::new(DownloadProjectReporter::new(printer, remote_count));

    let mut report = DownloadReport::default();
    let root_name = project.workspace().pyproject_toml().project.as_ref().map(|p| &p.name);
    let semaphore = concurrency.downloads_semaphore.clone();
    let mut tasks: JoinSet<Result<MaterializeOutcome>> = JoinSet::new();

    for (resolved, hashes) in resolution.hashes() {
        let ResolvedDist::Installable { dist, .. } = resolved else {
            continue;
        };
        match dist.as_ref() {
            Dist::Built(BuiltDist::Registry(built)) => {
                let wheel = built.best_wheel();
                let url = rewrite_registry_url(wheel.file.url.to_url()?, mirror_base.as_ref());
                let filename = sanitize_artifact_filename(wheel.file.filename.as_ref())?.to_owned();
                let dst = output_dir.join(&filename);
                // Prefer the per-file hashes published on the index; fall back to the
                // lock-level hashes (both are authoritative for registry wheels).
                let expected: Vec<HashDigest> = if wheel.file.hashes.is_empty() {
                    hashes.to_vec()
                } else {
                    wheel.file.hashes.to_vec()
                };
                spawn_download(&mut tasks, &client, &semaphore, &reporter, filename, url, dst, expected);
            }
            Dist::Built(BuiltDist::DirectUrl(direct)) => {
                let filename = sanitize_artifact_filename(&direct.filename.to_string())?.to_owned();
                let dst = output_dir.join(&filename);
                let url = (*direct.location).clone();
                let expected: Vec<HashDigest> = hashes.to_vec();
                spawn_download(&mut tasks, &client, &semaphore, &reporter, filename, url, dst, expected);
            }
            Dist::Built(BuiltDist::Path(local)) => {
                let dst = output_dir.join(local.filename.to_string());
                report.record(copy_or_link(&local.install_path, &dst)?);
            }
            Dist::Source(SourceDist::Registry(source)) => {
                let url = rewrite_registry_url(source.file.url.to_url()?, mirror_base.as_ref());
                let filename = sanitize_artifact_filename(source.file.filename.as_ref())?.to_owned();
                let dst = output_dir.join(&filename);
                let expected: Vec<HashDigest> = if source.file.hashes.is_empty() {
                    hashes.to_vec()
                } else {
                    source.file.hashes.to_vec()
                };
                spawn_download(&mut tasks, &client, &semaphore, &reporter, filename, url, dst, expected);
            }
            Dist::Source(SourceDist::DirectUrl(direct)) => {
                let raw = direct
                    .filename()
                    .ok()
                    .map(|f: std::borrow::Cow<'_, str>| f.into_owned())
                    .unwrap_or_else(|| format!("{}.{}", direct.name, direct.ext));
                let filename = sanitize_artifact_filename(&raw)?.to_owned();
                let dst = output_dir.join(&filename);
                let url = (*direct.location).clone();
                let expected: Vec<HashDigest> = hashes.to_vec();
                spawn_download(&mut tasks, &client, &semaphore, &reporter, filename, url, dst, expected);
            }
            Dist::Source(SourceDist::Git(git)) => {
                warn_user!(
                    "Skipping git source `{}` (not materialized into --output-dir)",
                    git.name
                );
            }
            Dist::Source(SourceDist::Path(path)) => {
                warn_user!(
                    "Skipping local path source `{}` (not materialized into --output-dir)",
                    path.name
                );
            }
            Dist::Source(SourceDist::Directory(dir)) => {
                // Suppress the warning for the root project and virtual workspace members;
                // they are expected to be skipped.
                let is_root = root_name.is_some_and(|n| n == &dir.name);
                let is_virtual = dir.r#virtual.unwrap_or(false);
                if !is_root && !is_virtual {
                    warn_user!(
                        "Skipping local/editable source `{}` (not materialized into --output-dir)",
                        dir.name
                    );
                }
            }
        }
    }

    // Drain spawned downloads. Abort remaining tasks on the first failure so we don't
    // leak half-written partials from unrelated downloads.
    while let Some(join_res) = tasks.join_next().await {
        match join_res {
            Ok(Ok(outcome)) => report.record(outcome),
            Ok(Err(err)) => {
                tasks.abort_all();
                return Err(err);
            }
            Err(join_err) => {
                tasks.abort_all();
                bail!("download task panicked: {join_err}");
            }
        }
    }

    reporter.on_complete();

    // 12. Print a summary. `already_existed` counts artifacts that were present
    // from a previous run and left untouched; dependencies that cannot be
    // materialized (git, local path, workspace members) surface their own
    // `Skipping ...` warnings above and are not included in either count.
    writeln!(
        printer.stderr(),
        "Downloaded {} package{} ({} already existed) to {}",
        report.written,
        if report.written == 1 { "" } else { "s" },
        report.already_existed,
        output_dir.display().cyan(),
    )?;

    Ok(ExitStatus::Success)
}

/// Build an [`InstallTarget`] for the download command.
///
/// Always targets the full workspace for [`VirtualProject::Project`] (equivalent
/// to `uv sync --all-packages`) because a wheelhouse is typically populated
/// across all members.
fn make_install_target<'a>(project: &'a VirtualProject, lock: &'a Lock) -> InstallTarget<'a> {
    match project {
        VirtualProject::Project(p) => InstallTarget::Workspace {
            workspace: p.workspace(),
            lock,
        },
        VirtualProject::NonProject(workspace) => {
            InstallTarget::NonProjectWorkspace { workspace, lock }
        }
    }
}

/// Outcome of materializing a single artifact.
#[derive(Clone, Copy)]
enum MaterializeOutcome {
    Written,
    /// The target file was already present from a previous run.
    AlreadyExisted,
}

/// Summary of a download run.
#[derive(Default)]
struct DownloadReport {
    written: usize,
    already_existed: usize,
}

impl DownloadReport {
    fn record(&mut self, outcome: MaterializeOutcome) {
        match outcome {
            MaterializeOutcome::Written => self.written += 1,
            MaterializeOutcome::AlreadyExisted => self.already_existed += 1,
        }
    }
}

/// Spawn a streaming download task onto `tasks`, gated by `semaphore`.
#[expect(clippy::too_many_arguments)]
fn spawn_download(
    tasks: &mut JoinSet<Result<MaterializeOutcome>>,
    client: &Arc<RegistryClient>,
    semaphore: &Arc<Semaphore>,
    reporter: &Arc<DownloadProjectReporter>,
    name: String,
    url: DisplaySafeUrl,
    dst: PathBuf,
    expected: Vec<HashDigest>,
) {
    let client = Arc::clone(client);
    let semaphore = Arc::clone(semaphore);
    let reporter = Arc::clone(reporter);
    tasks.spawn(async move {
        // `concurrency.downloads_semaphore` is constructed once in `Concurrency::new`
        // and lives for the duration of the command; it is not expected to close.
        // Surface the close as a real error rather than silently bypassing the
        // rate limit if a future refactor ever triggers it.
        let _permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("downloads semaphore was unexpectedly closed"))?;
        let outcome = download_to(&client, &reporter, name, url, &dst, &expected).await?;
        // Tick the root counter whether we actually downloaded or short-circuited on
        // an existing file, so `(pos/len)` reflects work finished, not bytes moved.
        reporter.on_task_done();
        Ok(outcome)
    });
}

/// RAII guard over a `.partial-<nonce>` file.
///
/// Removes the partial file when dropped — including when a spawned task is
/// cancelled mid-download via `JoinSet::abort_all()`. Without this guard, an
/// aborted task leaves its partial behind because the explicit cleanup branches
/// in `download_to` never run once the future is dropped at an `.await` point.
///
/// After a successful rename the partial path no longer exists, so the drop-time
/// `remove_file` call is a harmless no-op.
struct PartialFile(PathBuf);

impl PartialFile {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = fs_err::remove_file(&self.0);
    }
}

/// Stream a remote URL directly to `dst`, verifying hashes when present.
///
/// The response body is chunked to disk via `bytes_stream()` so we never hold the
/// full artifact in memory; hashes are updated incrementally as each chunk arrives.
/// Bytes land in a `.partial-<nonce>` sibling first and are renamed on success. A
/// [`PartialFile`] RAII guard removes the partial on any early return — including
/// async cancellation — and becomes a no-op once the file has been renamed.
async fn download_to(
    client: &RegistryClient,
    reporter: &DownloadProjectReporter,
    name: String,
    url: DisplaySafeUrl,
    dst: &Path,
    expected_hashes: &[HashDigest],
) -> Result<MaterializeOutcome> {
    // Only treat an existing regular file as already-materialized. Directories,
    // symlinks, or other exotica are an error so we don't silently skip them.
    match fs_err::symlink_metadata(dst) {
        Ok(metadata) if metadata.is_file() => return Ok(MaterializeOutcome::AlreadyExisted),
        Ok(_) => bail!(
            "refusing to overwrite non-file entry at `{}`",
            dst.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to stat `{}`: {err}",
                dst.display()
            ));
        }
    }

    let partial = PartialFile(
        dst.with_extension(format!("partial-{}", Uuid::new_v4().as_simple())),
    );

    let response = client
        .uncached_client(&url)
        .get(url.as_str())
        .send()
        .await
        .map_err(|err| anyhow::anyhow!("failed to fetch `{url}`: {err}"))?;

    let status = response.status();
    if !status.is_success() {
        bail!("failed to fetch `{url}`: HTTP {status}");
    }

    // Open the per-artifact progress bar once the server commits to a `Content-Length`
    // (missing for chunked/compressed responses — the bar then falls back to a spinner).
    let size = response.content_length();
    let progress_id = reporter.on_download_start(name, size);

    let mut hashers: Vec<Hasher> = expected_hashes
        .iter()
        .map(|h| Hasher::from(h.algorithm))
        .collect();

    let mut file = fs_err::tokio::File::create(partial.path())
        .await
        .map_err(|err| {
            anyhow::anyhow!("failed to create `{}`: {err}", partial.path().display())
        })?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|err| anyhow::anyhow!("failed to read body of `{url}`: {err}"))?;
        for hasher in &mut hashers {
            hasher.update(&chunk);
        }
        file.write_all(&chunk).await.map_err(|err| {
            anyhow::anyhow!("failed to write `{}`: {err}", partial.path().display())
        })?;
        reporter.on_download_progress(progress_id, chunk.len() as u64);
    }

    file.flush().await.map_err(|err| {
        anyhow::anyhow!("failed to flush `{}`: {err}", partial.path().display())
    })?;
    drop(file);

    // Verify hashes before renaming.
    for (expected, hasher) in expected_hashes.iter().zip(hashers) {
        let actual: uv_pypi_types::HashDigest = hasher.into();
        if actual.digest != expected.digest {
            bail!(
                "hash mismatch for `{url}`:\n  expected {}: {}\n  actual   {}: {}",
                expected.algorithm,
                expected.digest,
                actual.algorithm,
                actual.digest,
            );
        }
    }

    fs_err::rename(partial.path(), dst)
        .map_err(|err| anyhow::anyhow!("failed to finalize `{}`: {err}", dst.display()))?;

    reporter.on_download_complete(progress_id);

    Ok(MaterializeOutcome::Written)
}

/// Accept an artifact filename only if it is a single harmless path segment.
///
/// Direct URL sources derive their filename from the (percent-decoded) last path segment
/// of the remote URL. That segment can in principle contain path separators or traversal
/// markers if the URL is malicious or malformed; joining such a name onto `--output-dir`
/// would let us write outside the requested directory.
fn sanitize_artifact_filename(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        bail!("refusing to materialize artifact with empty or traversal filename: `{raw}`");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        bail!("refusing to materialize artifact with path separator in filename: `{raw}`");
    }
    Ok(trimmed)
}

/// The host served by PyPI for distribution artifacts (wheels, sdists).
///
/// Narrows [`rewrite_registry_url`] to the one host we know bandersnatch-style mirrors
/// actually clone from. Lockfiles with artifact URLs pointing at a custom/corporate index
/// with its own layout are left untouched — rewriting them to `{mirror}/packages/...`
/// would silently 404 and the user would have no way to tell what happened.
const PYPI_FILE_HOST: &str = "files.pythonhosted.org";

/// Rewrite a PyPI-hosted Registry artifact URL to point at a user-specified mirror.
///
/// Called only by `uv download`, and only in memory — the `uv.lock` file on disk is never
/// modified. Rewriting triggers only when:
///   1. the caller passed a `mirror_base` (i.e. `--default-index` was set to something
///      other than the built-in PyPI),
///   2. the original URL's host is [`PYPI_FILE_HOST`], and
///   3. the original URL's path contains `/packages/` (the standard layout shared by PyPI
///      and its bandersnatch-style mirrors — Tsinghua, USTC, Aliyun).
///
/// The resulting URL is `{mirror_base}/packages/...` with the original query and fragment
/// preserved so per-file hash fragments (`#sha256=...`) continue to round-trip. The SHA-256
/// digest recorded in the lockfile is still verified against the downloaded bytes, so a
/// misconfigured mirror surfaces as a hash mismatch rather than a silent substitution.
fn rewrite_registry_url(
    original: DisplaySafeUrl,
    mirror_base: Option<&DisplaySafeUrl>,
) -> DisplaySafeUrl {
    let Some(base) = mirror_base else {
        return original;
    };
    if original.host_str() != Some(PYPI_FILE_HOST) {
        return original;
    }
    let Some(idx) = original.path().find("/packages/") else {
        return original;
    };
    let packages_suffix = original.path()[idx..].to_owned();

    // Clone the mirror base and graft on the `/packages/...` path from the original URL.
    // Going through `url::Url::set_*` (rather than string concatenation) keeps auth,
    // port, and percent-encoding handled by the crate instead of us.
    let mut rewritten = base.clone();
    let trimmed_base_path = rewritten.path().trim_end_matches('/').to_owned();
    rewritten.set_path(&format!("{trimmed_base_path}{packages_suffix}"));
    rewritten.set_query(original.query());
    rewritten.set_fragment(original.fragment());
    rewritten
}

/// Hard-link or copy a local path artifact into the output directory.
fn copy_or_link(src: &Path, dst: &Path) -> Result<MaterializeOutcome> {
    match fs_err::symlink_metadata(dst) {
        Ok(metadata) if metadata.is_file() => return Ok(MaterializeOutcome::AlreadyExisted),
        Ok(_) => bail!(
            "refusing to overwrite non-file entry at `{}`",
            dst.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to stat `{}`: {err}",
                dst.display()
            ));
        }
    }
    if let Err(_link_err) = fs_err::hard_link(src, dst) {
        warn!(
            "hard_link {} -> {} failed; copying instead",
            src.display(),
            dst.display()
        );
        fs_err::copy(src, dst).map_err(|copy_err| {
            anyhow::anyhow!(
                "failed to copy `{}` to `{}`: {copy_err}",
                src.display(),
                dst.display(),
            )
        })?;
    }
    Ok(MaterializeOutcome::Written)
}

#[cfg(test)]
mod tests {
    use super::{rewrite_registry_url, sanitize_artifact_filename};
    use uv_redacted::DisplaySafeUrl;

    fn url(s: &str) -> DisplaySafeUrl {
        DisplaySafeUrl::parse(s).unwrap()
    }

    #[test]
    fn rewrite_passthrough_without_mirror() {
        let original =
            url("https://files.pythonhosted.org/packages/aa/bb/foo-1.0-py3-none-any.whl");
        let rewritten = rewrite_registry_url(original.clone(), None);
        assert_eq!(rewritten.as_str(), original.as_str());
    }

    #[test]
    fn rewrite_ustc_mirror() {
        let original = url(
            "https://files.pythonhosted.org/packages/aa/bb/cc/foo-1.0-py3-none-any.whl#sha256=deadbeef",
        );
        let mirror = url("https://mirrors.ustc.edu.cn/pypi/");
        let rewritten = rewrite_registry_url(original, Some(&mirror));
        assert_eq!(
            rewritten.as_str(),
            "https://mirrors.ustc.edu.cn/pypi/packages/aa/bb/cc/foo-1.0-py3-none-any.whl#sha256=deadbeef",
        );
    }

    #[test]
    fn rewrite_tsinghua_mirror_strips_trailing_slash() {
        let original =
            url("https://files.pythonhosted.org/packages/aa/bb/foo-1.0.tar.gz");
        // `IndexUrl::root()` normally returns a URL without a trailing slash; accept either
        // form defensively so manual callers (and tests) don't need to care.
        let mirror = url("https://pypi.tuna.tsinghua.edu.cn/");
        let rewritten = rewrite_registry_url(original, Some(&mirror));
        assert_eq!(
            rewritten.as_str(),
            "https://pypi.tuna.tsinghua.edu.cn/packages/aa/bb/foo-1.0.tar.gz",
        );
    }

    #[test]
    fn rewrite_preserves_query() {
        let original = url(
            "https://files.pythonhosted.org/packages/aa/bb/foo-1.0.tar.gz?token=abc#sha256=beef",
        );
        let mirror = url("https://mirrors.ustc.edu.cn/pypi");
        let rewritten = rewrite_registry_url(original, Some(&mirror));
        assert_eq!(
            rewritten.as_str(),
            "https://mirrors.ustc.edu.cn/pypi/packages/aa/bb/foo-1.0.tar.gz?token=abc#sha256=beef",
        );
    }

    #[test]
    fn rewrite_skips_non_pypi_style_paths() {
        // A URL without `/packages/` in the path (e.g. a custom index that uses a different
        // layout) is left untouched — we don't have enough information to know where the
        // mirror would serve the file.
        let original = url("https://files.pythonhosted.org/wheels/foo-1.0-py3-none-any.whl");
        let mirror = url("https://mirrors.ustc.edu.cn/pypi");
        let rewritten = rewrite_registry_url(original.clone(), Some(&mirror));
        assert_eq!(rewritten.as_str(), original.as_str());
    }

    #[test]
    fn rewrite_skips_non_pypi_hosts() {
        // URLs from corporate/custom indexes (anything other than files.pythonhosted.org)
        // are left alone. Rewriting them to `{mirror}/packages/...` would silently 404
        // because mirrors clone from PyPI, not from the custom index.
        let original = url("https://corp.example.com/artifactory/pypi/packages/foo.whl");
        let mirror = url("https://mirrors.ustc.edu.cn/pypi");
        let rewritten = rewrite_registry_url(original.clone(), Some(&mirror));
        assert_eq!(rewritten.as_str(), original.as_str());
    }

    #[test]
    fn rewrite_end_to_end_via_index_url_root() {
        // Drive the full pipeline: IndexUrl::parse → .root() → rewrite. This catches
        // behavior changes in `IndexUrl::root()` or in how `DisplaySafeUrl` round-trips
        // the trailing slash after `pop_if_empty().pop()`.
        use uv_distribution_types::IndexUrl;

        // With trailing slash on --default-index.
        let index = IndexUrl::parse("https://mirrors.ustc.edu.cn/pypi/simple/", None).unwrap();
        let base = index.root().unwrap();
        let original = url(
            "https://files.pythonhosted.org/packages/aa/bb/cc/foo-1.0-py3-none-any.whl",
        );
        let rewritten = rewrite_registry_url(original, Some(&base));
        assert_eq!(
            rewritten.as_str(),
            "https://mirrors.ustc.edu.cn/pypi/packages/aa/bb/cc/foo-1.0-py3-none-any.whl",
        );

        // Without trailing slash on --default-index.
        let index = IndexUrl::parse("https://mirrors.ustc.edu.cn/pypi/simple", None).unwrap();
        let base = index.root().unwrap();
        let original = url(
            "https://files.pythonhosted.org/packages/aa/bb/cc/foo-1.0-py3-none-any.whl",
        );
        let rewritten = rewrite_registry_url(original, Some(&base));
        assert_eq!(
            rewritten.as_str(),
            "https://mirrors.ustc.edu.cn/pypi/packages/aa/bb/cc/foo-1.0-py3-none-any.whl",
        );
    }

    #[test]
    fn sanitize_accepts_normal_filenames() {
        assert_eq!(
            sanitize_artifact_filename("iniconfig-2.0.0-py3-none-any.whl").unwrap(),
            "iniconfig-2.0.0-py3-none-any.whl",
        );
        assert_eq!(
            sanitize_artifact_filename("  foo-1.0.tar.gz  ").unwrap(),
            "foo-1.0.tar.gz",
        );
    }

    #[test]
    fn sanitize_rejects_path_separators() {
        assert!(sanitize_artifact_filename("../secret.whl").is_err());
        assert!(sanitize_artifact_filename("dir/inner.whl").is_err());
        assert!(sanitize_artifact_filename("dir\\inner.whl").is_err());
    }

    #[test]
    fn sanitize_rejects_empty_and_dot_filenames() {
        assert!(sanitize_artifact_filename("").is_err());
        assert!(sanitize_artifact_filename("   ").is_err());
        assert!(sanitize_artifact_filename(".").is_err());
        assert!(sanitize_artifact_filename("..").is_err());
    }
}

use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use owo_colors::OwoColorize;
use tracing::warn;
use uuid::Uuid;

use uv_cache::Cache;
use uv_client::{BaseClientBuilder, RegistryClientBuilder};
use uv_configuration::{
    Concurrency, DependencyGroups, DependencyGroupsWithDefaults, ExtrasSpecification,
    InstallOptions, PlatformOs, PlatformSpec, PyImpl, TargetTriple,
};
use uv_distribution_types::{
    BuiltDist, Dist, RemoteSource, ResolvedDist, SourceDist,
};
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

    let client = RegistryClientBuilder::new(client_builder.clone(), cache.clone())
        .index_locations(index_locations.clone())
        .index_strategy(index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build()?;

    // 10. Ensure the output directory exists.
    fs_err::create_dir_all(&output_dir)?;

    // 11. Walk the resolution and directly download each artifact.
    let mut report = DownloadReport::default();
    let root_name = project.workspace().pyproject_toml().project.as_ref().map(|p| &p.name);

    for (resolved, hashes) in resolution.hashes() {
        let ResolvedDist::Installable { dist, .. } = resolved else {
            continue;
        };
        match dist.as_ref() {
            Dist::Built(BuiltDist::Registry(built)) => {
                let wheel = built.best_wheel();
                let url = wheel.file.url.to_url()?;
                let filename = sanitize_artifact_filename(wheel.file.filename.as_ref())?;
                let dst = output_dir.join(filename);
                // Prefer the per-file hashes published on the index; fall back to the
                // lock-level hashes (both are authoritative for registry wheels).
                let expected = if wheel.file.hashes.is_empty() {
                    hashes
                } else {
                    wheel.file.hashes.as_slice()
                };
                download_to(&client, url, &dst, expected, &mut report).await?;
            }
            Dist::Built(BuiltDist::DirectUrl(direct)) => {
                let dst = output_dir.join(direct.filename.to_string());
                download_to(
                    &client,
                    (*direct.location).clone(),
                    &dst,
                    hashes,
                    &mut report,
                )
                .await?;
            }
            Dist::Built(BuiltDist::Path(local)) => {
                let dst = output_dir.join(local.filename.to_string());
                copy_or_link(&local.install_path, &dst, &mut report)?;
            }
            Dist::Source(SourceDist::Registry(source)) => {
                let url = source.file.url.to_url()?;
                let filename = sanitize_artifact_filename(source.file.filename.as_ref())?;
                let dst = output_dir.join(filename);
                let expected = if source.file.hashes.is_empty() {
                    hashes
                } else {
                    source.file.hashes.as_slice()
                };
                download_to(&client, url, &dst, expected, &mut report).await?;
            }
            Dist::Source(SourceDist::DirectUrl(direct)) => {
                let raw = direct
                    .filename()
                    .ok()
                    .map(|f: std::borrow::Cow<'_, str>| f.into_owned())
                    .unwrap_or_else(|| format!("{}.{}", direct.name, direct.ext));
                let filename = sanitize_artifact_filename(&raw)?;
                let dst = output_dir.join(filename);
                download_to(
                    &client,
                    (*direct.location).clone(),
                    &dst,
                    hashes,
                    &mut report,
                )
                .await?;
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

    // 12. Print a summary.
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

/// Build an [`InstallTarget`] for the download command.
///
/// Always targets the full workspace for [`VirtualProject::Project`] (equivalent
/// to `uv sync --all-packages`) because a wheelhouse is typically populated
/// across all members. If single-root or package-selected
/// materialization is needed later, this is the place to thread a filter.
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

/// Summary of a download run.
#[derive(Default)]
struct DownloadReport {
    written: usize,
    skipped: usize,
}

/// Stream a remote URL directly to `dst`, verifying hashes when present.
///
/// Uses an atomic write: bytes land in a `.partial-<nonce>` sibling, then are renamed
/// on success.  On any failure the partial file is removed and the error is propagated.
async fn download_to(
    client: &uv_client::RegistryClient,
    url: DisplaySafeUrl,
    dst: &Path,
    expected_hashes: &[HashDigest],
    report: &mut DownloadReport,
) -> Result<()> {
    if dst.exists() {
        report.skipped += 1;
        return Ok(());
    }

    let partial = dst.with_extension(format!(
        "partial-{}",
        Uuid::new_v4().as_simple()
    ));

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

    // Build hashers for every algorithm referenced in expected_hashes.
    let mut hashers: Vec<Hasher> = expected_hashes
        .iter()
        .map(|h| Hasher::from(h.algorithm))
        .collect();

    let body = response
        .bytes()
        .await
        .map_err(|err| anyhow::anyhow!("failed to read body of `{url}`: {err}"))?;

    for hasher in &mut hashers {
        hasher.update(&body);
    }

    // Verify hashes before writing to disk.
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

    // TODO: stream the response body (`response.bytes_stream()`) to disk and hash it
    // incrementally. The current `response.bytes().await` pulls the full artifact into
    // memory, which is fine for typical wheels but wasteful for large ML distributions.

    // Write to a partial file first for atomicity.
    if let Err(err) = fs_err::write(&partial, &body) {
        let _ = fs_err::remove_file(&partial);
        return Err(anyhow::anyhow!(
            "failed to write `{}`: {err}",
            partial.display()
        ));
    }

    if let Err(err) = fs_err::rename(&partial, dst) {
        let _ = fs_err::remove_file(&partial);
        return Err(anyhow::anyhow!(
            "failed to finalize `{}`: {err}",
            dst.display()
        ));
    }

    report.written += 1;
    Ok(())
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

/// Hard-link or copy a local path artifact into the output directory.
fn copy_or_link(src: &Path, dst: &Path, report: &mut DownloadReport) -> Result<()> {
    if dst.exists() {
        report.skipped += 1;
        return Ok(());
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
    report.written += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sanitize_artifact_filename;

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

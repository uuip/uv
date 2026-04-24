use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use owo_colors::OwoColorize;
use rustc_hash::FxHashSet;
use tracing::{debug, warn};
use walkdir::WalkDir;
use zip::ZipWriter;
use zip::write::FileOptions;

use uv_cache::Cache;
use uv_client::{BaseClientBuilder, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{
    Concurrency, DependencyGroups, DependencyGroupsWithDefaults, ExtrasSpecification,
    HashCheckingMode, InstallOptions, PlatformOs, PlatformSpec, PyImpl, TargetTriple,
};
use uv_dispatch::BuildDispatch;
use uv_distribution::LoweredExtraBuildDependencies;
use uv_distribution_types::{CachedDist, Dist, Index, Resolution, ResolvedDist, SourceDist};
use uv_normalize::DefaultExtras;
use uv_platform_tags::Arch;
use uv_preview::Preview;
use uv_python::{Interpreter, PythonDownloads, PythonPreference, PythonRequest};
use uv_resolver::{FlatIndex, Installable, Lock};
use uv_settings::PythonInstallMirrors;
use uv_types::{BuildIsolation, HashStrategy, InFlight, SourceTreeEditablePolicy};
use uv_warnings::warn_user;
use uv_workspace::{DiscoveryOptions, MemberDiscovery, VirtualProject, WorkspaceCache};

use crate::commands::pip::loggers::DefaultResolveLogger;
use crate::commands::pip::{operations, resolution_markers, resolution_tags};
use crate::commands::project::install_target::InstallTarget;
use crate::commands::project::lock::{LockMode, LockOperation};
use crate::commands::project::lock_target::LockTarget;
use crate::commands::project::{
    ProjectError, ProjectInterpreter, UniversalState, WorkspacePython, default_dependency_groups,
};
use crate::commands::{ExitStatus, diagnostics};
use crate::printer::Printer;
use crate::settings::{FrozenSource, LockCheck, ResolverInstallerSettings};

/// Download the project's pinned dependencies as wheel files into `output_dir`.
///
/// Resolves (or reads) the lockfile for the requested target platform, downloads all required
/// wheels into uv's cache, and then materializes them as `.whl` archives in the output directory.
/// No virtual environment is created or modified.
#[expect(clippy::too_many_arguments)]
#[expect(dead_code, reason = "wired in Task 6")]
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

    // 9. Filter out local path / editable / git sources; they cannot be packaged as wheel archives.
    let resolution = filter_local_sources(resolution);

    // 10. Build RegistryClient + FlatIndex + BuildDispatch (mirrors do_sync in sync.rs).
    let index_locations = &settings.resolver.index_locations;
    let index_strategy = settings.resolver.index_strategy;
    let keyring_provider = settings.resolver.keyring_provider;
    let dependency_metadata = &settings.resolver.dependency_metadata;
    let config_setting = &settings.resolver.config_setting;
    let config_settings_package = &settings.resolver.config_settings_package;
    let exclude_newer = &settings.resolver.exclude_newer;
    let link_mode = settings.resolver.link_mode;
    let build_options = &settings.resolver.build_options;
    let sources = settings.resolver.sources;
    let extra_build_dependencies = &settings.resolver.extra_build_dependencies;
    let extra_build_variables = &settings.resolver.extra_build_variables;

    let extra_build_requires = LoweredExtraBuildDependencies::from_workspace(
        extra_build_dependencies.clone(),
        project.workspace(),
        index_locations,
        &sources,
        client_builder.credentials_cache(),
    )?
    .into_inner();

    let client_builder = client_builder.clone().keyring(keyring_provider);

    let build_hasher = HashStrategy::default();
    let hasher = HashStrategy::from_resolution(&resolution, HashCheckingMode::Verify)?;

    let client = RegistryClientBuilder::new(client_builder.clone(), cache.clone())
        .index_locations(index_locations.clone())
        .index_strategy(index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build()?;

    let flat_index = {
        let flat_client =
            FlatIndexClient::new(client.cached_client(), client.connectivity(), cache);
        let entries = flat_client
            .fetch_all(index_locations.flat_indexes().map(Index::url))
            .await?;
        FlatIndex::from_entries(entries, Some(&tags), &hasher, build_options)
    };

    let build_constraints = install_target.build_constraints();

    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        &build_constraints,
        interpreter,
        index_locations,
        &flat_index,
        dependency_metadata,
        state.fork().into_inner(),
        index_strategy,
        config_setting,
        config_settings_package,
        BuildIsolation::Isolated,
        &extra_build_requires,
        extra_build_variables,
        link_mode,
        build_options,
        &build_hasher,
        exclude_newer.clone(),
        sources,
        SourceTreeEditablePolicy::Project,
        workspace_cache.clone(),
        concurrency.clone(),
        preview,
    );

    // 11. Collect all installable distributions and prepare (download + unzip into cache).
    let all_dists: Vec<Arc<Dist>> = resolution
        .distributions()
        .filter_map(|dist| {
            if let ResolvedDist::Installable { dist, .. } = dist {
                Some(dist.clone())
            } else {
                None
            }
        })
        .collect();

    let in_flight = InFlight::default();
    let cached = match operations::prepare(
        all_dists,
        &in_flight,
        &resolution,
        &hasher,
        &tags,
        build_options,
        &client,
        &build_dispatch,
        cache,
        &concurrency,
        printer,
    )
    .await
    {
        Ok(cached) => cached,
        Err(err) => {
            return diagnostics::OperationDiagnostic::with_system_certs(
                client_builder.system_certs(),
            )
            .report(err)
            .map_or(Ok(ExitStatus::Failure), |err| Err(err.into()));
        }
    };

    // 12. Materialize cached (unzipped) wheels as .whl archives in the output directory.
    let report = materialize_to_out(&cached, &output_dir)?;

    // 13. Print a summary.
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
/// across all members. Spec §4.3. If single-root or package-selected
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

/// Filter out distributions sourced from local directories (editable/path) or git, which cannot
/// be materialized as standalone wheel archives.
fn filter_local_sources(resolution: Resolution) -> Resolution {
    resolution.filter(|dist| {
        let ResolvedDist::Installable { dist, .. } = dist else {
            return true;
        };
        match dist.as_ref() {
            Dist::Source(SourceDist::Directory(d)) => {
                warn_user!(
                    "Skipping local/editable source `{}` (not materialized into --output-dir)",
                    d.name
                );
                false
            }
            Dist::Source(SourceDist::Git(g)) => {
                warn_user!(
                    "Skipping git source `{}` (not materialized into --output-dir)",
                    g.name
                );
                false
            }
            Dist::Source(SourceDist::Path(p)) => {
                warn_user!(
                    "Skipping local path source `{}` (not materialized into --output-dir)",
                    p.name
                );
                false
            }
            _ => true,
        }
    })
}

/// Summary of a [`materialize_to_out`] run.
#[derive(Default)]
struct DownloadReport {
    written: usize,
    skipped: usize,
}

/// Materialize the given [`CachedDist`] entries into `out_dir` as `.whl` files.
///
/// uv's cache holds wheels in extracted form, so this function re-archives each
/// cache directory into a `.whl` using DEFLATE compression.
///
/// # Known limitation
///
/// The re-zipped output is functionally equivalent to the upstream wheel, but
/// its SHA-256 will NOT match the hash stored in `uv.lock` or published on
/// PyPI. Downstream tools that re-verify wheels against those hashes
/// (`pip install --require-hashes`, `pip-audit`, etc.) will reject the output.
///
/// TODO: Teach `operations::prepare` (or `DistributionDatabase`) to retain the
/// original wheel bytes alongside the extracted archive, then hard-link from
/// there into `out_dir` instead of re-archiving.
fn materialize_to_out(cached: &[CachedDist], out_dir: &Path) -> Result<DownloadReport> {
    fs_err::create_dir_all(out_dir)?;

    let mut report = DownloadReport::default();
    let mut seen: FxHashSet<String> = FxHashSet::default();

    for dist in cached {
        // Use the authoritative filename stored in the CachedDist metadata.
        let whl_name = match dist {
            CachedDist::Registry(r) => r.filename.to_string(),
            CachedDist::Url(u) => u.filename.to_string(),
        };

        if !seen.insert(whl_name.clone()) {
            debug!("Skipping duplicate wheel {whl_name}");
            continue;
        }

        let dst = out_dir.join(&whl_name);
        if dst.exists() {
            report.skipped += 1;
            continue;
        }

        let src = dist.path();

        if src.is_dir() {
            // Re-zip the unzipped wheel directory into a .whl (ZIP) archive.
            let file = fs_err::File::create(&dst)
                .map_err(|e| anyhow::anyhow!("failed to create `{}`: {e}", dst.display()))?;
            let mut zip = ZipWriter::new(file);
            let options: FileOptions<'_, ()> = FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            for entry in WalkDir::new(src).sort_by_file_name() {
                let entry = entry.map_err(|e| {
                    anyhow::anyhow!("error reading wheel directory `{}`: {e}", src.display())
                })?;
                let path = entry.path();
                let relative = path.strip_prefix(src).map_err(|_| {
                    anyhow::anyhow!(
                        "WalkDir yielded path `{}` not under root `{}`",
                        path.display(),
                        src.display()
                    )
                })?;

                let name = relative.to_str().ok_or_else(|| {
                    anyhow::anyhow!("non-UTF-8 path inside wheel: {}", path.display())
                })?;

                if entry.file_type().is_dir() {
                    if !name.is_empty() {
                        zip.add_directory(name, options).map_err(|e| {
                            anyhow::anyhow!("failed to add dir `{name}` to zip: {e}")
                        })?;
                    }
                } else {
                    zip.start_file(name, options).map_err(|e| {
                        anyhow::anyhow!("failed to add file `{name}` to zip: {e}")
                    })?;
                    let mut f = fs_err::File::open(path)?;
                    std::io::copy(&mut f, &mut zip)?;
                }
            }

            zip.finish().map_err(|e| {
                anyhow::anyhow!("failed to finalize zip `{}`: {e}", dst.display())
            })?;
        } else {
            // The source is already a plain file; prefer hard-link, fall back to fs_err copy.
            if let Err(link_err) = fs_err::hard_link(src, &dst) {
                warn!(
                    "hard_link {} -> {} failed ({link_err}); copying",
                    src.display(),
                    dst.display()
                );
                fs_err::copy(src, &dst).map_err(|copy_err| {
                    anyhow::anyhow!(
                        "failed to materialize `{}` into `{}`: \
                         hard_link error: {link_err}; copy error: {copy_err}",
                        src.display(),
                        dst.display(),
                    )
                })?;
            }
        }

        report.written += 1;
    }

    Ok(report)
}

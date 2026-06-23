use std::path::PathBuf;

use uv_cache::Refresh;
use uv_cli::{
    DownloadArgs, Maybe,
    options::{flag, resolve_flag_pair},
};
use uv_configuration::{DependencyGroups, DevMode, ExtrasSpecification, PlatformOs, PyImpl};
use uv_platform_tags::Arch;
use uv_settings::{EnvironmentOptions, FilesystemOptions, PythonInstallMirrors};

use super::{
    FrozenFlag, FrozenSource, LockCheck, LockedFlag, ResolverSettings, resolve_frozen,
    resolve_lock_check, resolve_lock_flags,
};

/// The resolved settings to use for a `download` invocation.
#[derive(Debug, Clone)]
pub(crate) struct DownloadSettings {
    pub(crate) lock_check: LockCheck,
    pub(crate) frozen: Option<FrozenSource>,
    pub(crate) extras: ExtrasSpecification,
    pub(crate) groups: DependencyGroups,
    pub(crate) output_dir: PathBuf,
    pub(crate) platform: PlatformOs,
    pub(crate) machine: Arch,
    pub(crate) glibc: Option<(u16, u16)>,
    pub(crate) implementation: PyImpl,
    pub(crate) python: Option<String>,
    pub(crate) install_mirrors: PythonInstallMirrors,
    pub(crate) settings: ResolverSettings,
    pub(crate) refresh: Refresh,
}

impl DownloadSettings {
    /// Resolve the [`DownloadSettings`] from the CLI and filesystem configuration.
    pub(crate) fn resolve(
        args: DownloadArgs,
        filesystem: Option<FilesystemOptions>,
        environment: EnvironmentOptions,
    ) -> anyhow::Result<Self> {
        let DownloadArgs {
            extra,
            all_extras,
            no_extra,
            no_all_extras,
            dev,
            no_dev,
            only_dev,
            group,
            no_group,
            no_default_groups,
            only_group,
            all_groups,
            locked,
            frozen,
            resolver,
            build,
            refresh,
            python,
            platform,
            machine,
            glibc,
            implementation,
            output_dir,
        } = args;

        let filesystem_install_mirrors = filesystem
            .clone()
            .map(|fs| fs.install_mirrors.clone())
            .unwrap_or_default();

        let settings = ResolverSettings::resolve(resolver, build, filesystem, &environment)?;

        let (dev, no_dev) = resolve_flag_pair(
            dev,
            no_dev,
            "dev",
            "no-dev",
            Some(environment.dev),
            Some(environment.no_dev),
        );

        // Resolve flags from CLI and environment variables.
        let locked = resolve_lock_check(locked, false, LockedFlag::Locked, environment.locked);
        let frozen = resolve_frozen(frozen, false, FrozenFlag::Frozen, environment.frozen);
        let (locked, frozen) = resolve_lock_flags(locked, frozen)?;

        Ok(Self {
            lock_check: locked,
            frozen,
            extras: ExtrasSpecification::from_args(
                extra.unwrap_or_default(),
                no_extra,
                false,
                vec![],
                flag(all_extras, no_all_extras, "all-extras")?.unwrap_or_default(),
            ),
            groups: DependencyGroups::from_args(
                DevMode::from_args(dev.into(), no_dev.into(), only_dev),
                group,
                if no_group.is_empty() {
                    environment.no_group.clone().unwrap_or_default()
                } else {
                    no_group
                },
                no_default_groups,
                only_group,
                all_groups,
            ),
            output_dir,
            platform: platform.unwrap_or_else(host_platform_os),
            machine: machine.unwrap_or_else(host_platform_machine),
            glibc,
            implementation,
            python: python.and_then(Maybe::into_option),
            refresh: Refresh::try_from(refresh)?,
            settings,
            install_mirrors: environment
                .install_mirrors
                .combine(filesystem_install_mirrors),
        })
    }
}

fn host_platform_os() -> PlatformOs {
    #[cfg(target_os = "linux")]
    {
        PlatformOs::Linux
    }
    #[cfg(target_os = "windows")]
    {
        PlatformOs::Windows
    }
    #[cfg(target_os = "macos")]
    {
        PlatformOs::Macos
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        compile_error!(
            "uv download: host target_os must be one of `linux`, `windows`, `macos`; \
             the download command needs a host-platform default for `--platform`."
        )
    }
}

fn host_platform_machine() -> Arch {
    #[cfg(target_arch = "x86_64")]
    {
        Arch::X86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        Arch::Aarch64
    }
    #[cfg(target_arch = "x86")]
    {
        Arch::X86
    }
    #[cfg(target_arch = "riscv64")]
    {
        Arch::Riscv64
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "riscv64"
    )))]
    {
        compile_error!(
            "uv download: host target_arch must be one of `x86_64`, `aarch64`, `x86`, `riscv64`; \
             the download command needs a host-machine default for `--machine`."
        )
    }
}

use std::fmt;
use std::str::FromStr;

use uv_platform_tags::Arch;

use crate::TargetTriple;

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

impl fmt::Display for PlatformOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Macos => "macos",
        };
        f.write_str(name)
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

/// Parse a user-supplied `--machine` value into an [`Arch`].
pub fn parse_machine(raw: &str) -> Result<Arch, String> {
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "amd64" | "x86_64" | "x86-64" | "x64" => Ok(Arch::X86_64),
        "arm64" | "aarch64" => Ok(Arch::Aarch64),
        "i386" | "i686" | "x86" => Ok(Arch::X86),
        other => Arch::from_str(other).map_err(|_| {
            format!(
                "unknown machine `{raw}`; expected one of `x86_64`, `amd64`, `aarch64`, `arm64`, `i686`, \u{2026}"
            )
        }),
    }
}

/// Parse `MAJOR.MINOR` or `MAJOR_MINOR` into a glibc `(major, minor)` tuple.
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

/// Normalized, validated target description for `uv download`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PlatformSpec {
    pub os: PlatformOs,
    pub arch: Arch,
    pub glibc: Option<(u16, u16)>,
    pub implementation: PyImpl,
}

/// Errors produced while building or evaluating a [`PlatformSpec`].
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum PlatformSpecError {
    #[error("--glibc is only valid with --platform=linux")]
    GlibcOnNonLinux,
    #[error("only CPython is supported in this release")]
    UnsupportedImplementation,
    #[error("{os} + {arch} is not a supported target platform")]
    UnsupportedCombination { os: PlatformOs, arch: Arch },
    #[error("manylinux_2_{minor} is not supported for {arch} (supported minors: {supported:?})")]
    UnsupportedGlibc {
        arch: Arch,
        minor: u16,
        supported: &'static [u16],
    },
}

impl PlatformSpec {
    /// Build a [`PlatformSpec`] from optional CLI values and host defaults.
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

    /// Map this spec to an existing [`TargetTriple`] used by the resolver.
    pub fn to_target_triple(self) -> Result<TargetTriple, PlatformSpecError> {
        use Arch::{Aarch64, Riscv64, X86, X86_64};
        use PlatformOs::{Linux, Macos, Windows};
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
                    supported: &[17, 28, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40],
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
                    supported: &[17, 28, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40],
                }),
            },
            (Linux, X86_64, Some((major, minor))) if major != 2 => {
                Err(PlatformSpecError::UnsupportedGlibc {
                    arch: X86_64,
                    minor,
                    supported: &[17, 28, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40],
                })
            }
            (Linux, Aarch64, Some((major, minor))) if major != 2 => {
                Err(PlatformSpecError::UnsupportedGlibc {
                    arch: Aarch64,
                    minor,
                    supported: &[17, 28, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40],
                })
            }
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
        let spec =
            PlatformSpec::from_parts(None, None, None, None, PlatformOs::Macos, Arch::Aarch64)
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
        assert_eq!(spec.to_target_triple(), Ok(TargetTriple::Aarch64Manylinux228));
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
        assert_eq!(mac.to_target_triple(), Ok(TargetTriple::Aarch64AppleDarwin));
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

    #[test]
    fn target_triple_rejects_glibc_major_other_than_2() {
        let spec = PlatformSpec {
            os: PlatformOs::Linux,
            arch: Arch::X86_64,
            glibc: Some((3, 0)),
            implementation: PyImpl::CPython,
        };
        assert!(matches!(
            spec.to_target_triple(),
            Err(PlatformSpecError::UnsupportedGlibc { minor: 0, .. })
        ));
    }

    #[test]
    fn target_triple_linux_riscv64() {
        let spec = PlatformSpec {
            os: PlatformOs::Linux,
            arch: Arch::Riscv64,
            glibc: Some((2, 39)),
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            spec.to_target_triple(),
            Ok(TargetTriple::Riscv64UnknownLinuxGnu),
        );
    }

    #[test]
    fn target_triple_windows_x86() {
        let spec = PlatformSpec {
            os: PlatformOs::Windows,
            arch: Arch::X86,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(spec.to_target_triple(), Ok(TargetTriple::I686PcWindowsMsvc));
    }

    #[test]
    fn target_triple_windows_aarch64() {
        let spec = PlatformSpec {
            os: PlatformOs::Windows,
            arch: Arch::Aarch64,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            spec.to_target_triple(),
            Ok(TargetTriple::Aarch64PcWindowsMsvc),
        );
    }
}

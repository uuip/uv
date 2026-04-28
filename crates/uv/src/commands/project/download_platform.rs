//! Mapping of user-facing `uv download --platform/--machine/--glibc/--implementation`
//! values into a resolver [`TargetTriple`].
//!
//! The raw CLI value types and their parsers stay in `uv-configuration` so `uv-cli`
//! can reach them as `clap` `value_parser` functions. The composition / validation
//! logic — which is only used by `uv download` — lives here.

use uv_configuration::{PlatformOs, PyImpl, TargetTriple};
use uv_platform_tags::Arch;

/// Normalized, validated target description for `uv download`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PlatformSpec {
    pub(crate) os: PlatformOs,
    pub(crate) arch: Arch,
    pub(crate) glibc: Option<(u16, u16)>,
    pub(crate) implementation: PyImpl,
}

/// Errors produced while building or evaluating a [`PlatformSpec`].
#[derive(Debug, PartialEq, thiserror::Error)]
pub(crate) enum PlatformSpecError {
    #[error("--glibc is only valid with --platform=linux")]
    GlibcOnNonLinux,
    #[error("{os} + {arch} is not a supported target platform")]
    UnsupportedCombination { os: PlatformOs, arch: Arch },
    #[error(
        "glibc {major}.{minor} is not supported for {arch} \
         (supported manylinux tags: 2.17, 2.28, 2.31–2.40)"
    )]
    UnsupportedGlibc {
        arch: Arch,
        major: u16,
        minor: u16,
    },
}

impl PlatformSpec {
    /// Validate a fully-specified target into a [`PlatformSpec`].
    ///
    /// Callers are expected to have already filled in host defaults for `os`/`arch`
    /// (see `host_platform_os` / `host_platform_machine` in `uv/src/settings.rs`).
    /// `glibc` stays `Option` because it has a genuine tri-state: explicit / defaulted
    /// to 2.28 on Linux / rejected on non-Linux.
    pub(crate) fn new(
        os: PlatformOs,
        arch: Arch,
        glibc: Option<(u16, u16)>,
        implementation: PyImpl,
    ) -> Result<Self, PlatformSpecError> {
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
    pub(crate) fn to_target_triple(self) -> Result<TargetTriple, PlatformSpecError> {
        use Arch::{Aarch64, Riscv64, X86, X86_64};
        use PlatformOs::{Linux, Macos, Windows};
        match (self.os, self.arch, self.glibc) {
            (Linux, arch @ (X86_64 | Aarch64), Some((2, minor))) => {
                linux_manylinux(arch, minor).ok_or(PlatformSpecError::UnsupportedGlibc {
                    arch,
                    major: 2,
                    minor,
                })
            }
            (Linux, arch @ (X86_64 | Aarch64), Some((major, minor))) => {
                Err(PlatformSpecError::UnsupportedGlibc {
                    arch,
                    major,
                    minor,
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

/// Map `(arch, minor)` to the corresponding manylinux 2.MINOR [`TargetTriple`] variant.
fn linux_manylinux(arch: Arch, minor: u16) -> Option<TargetTriple> {
    match (arch, minor) {
        (Arch::X86_64, 17) => Some(TargetTriple::X8664Manylinux217),
        (Arch::X86_64, 28) => Some(TargetTriple::X8664Manylinux228),
        (Arch::X86_64, 31) => Some(TargetTriple::X8664Manylinux231),
        (Arch::X86_64, 32) => Some(TargetTriple::X8664Manylinux232),
        (Arch::X86_64, 33) => Some(TargetTriple::X8664Manylinux233),
        (Arch::X86_64, 34) => Some(TargetTriple::X8664Manylinux234),
        (Arch::X86_64, 35) => Some(TargetTriple::X8664Manylinux235),
        (Arch::X86_64, 36) => Some(TargetTriple::X8664Manylinux236),
        (Arch::X86_64, 37) => Some(TargetTriple::X8664Manylinux237),
        (Arch::X86_64, 38) => Some(TargetTriple::X8664Manylinux238),
        (Arch::X86_64, 39) => Some(TargetTriple::X8664Manylinux239),
        (Arch::X86_64, 40) => Some(TargetTriple::X8664Manylinux240),
        (Arch::Aarch64, 17) => Some(TargetTriple::Aarch64Manylinux217),
        (Arch::Aarch64, 28) => Some(TargetTriple::Aarch64Manylinux228),
        (Arch::Aarch64, 31) => Some(TargetTriple::Aarch64Manylinux231),
        (Arch::Aarch64, 32) => Some(TargetTriple::Aarch64Manylinux232),
        (Arch::Aarch64, 33) => Some(TargetTriple::Aarch64Manylinux233),
        (Arch::Aarch64, 34) => Some(TargetTriple::Aarch64Manylinux234),
        (Arch::Aarch64, 35) => Some(TargetTriple::Aarch64Manylinux235),
        (Arch::Aarch64, 36) => Some(TargetTriple::Aarch64Manylinux236),
        (Arch::Aarch64, 37) => Some(TargetTriple::Aarch64Manylinux237),
        (Arch::Aarch64, 38) => Some(TargetTriple::Aarch64Manylinux238),
        (Arch::Aarch64, 39) => Some(TargetTriple::Aarch64Manylinux239),
        (Arch::Aarch64, 40) => Some(TargetTriple::Aarch64Manylinux240),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glibc_on_windows_is_rejected() {
        let err = PlatformSpec::new(
            PlatformOs::Windows,
            Arch::X86_64,
            Some((2, 28)),
            PyImpl::CPython,
        )
        .unwrap_err();
        assert!(matches!(err, PlatformSpecError::GlibcOnNonLinux));
    }

    #[test]
    fn glibc_on_macos_is_rejected() {
        let err = PlatformSpec::new(
            PlatformOs::Macos,
            Arch::Aarch64,
            Some((2, 28)),
            PyImpl::CPython,
        )
        .unwrap_err();
        assert!(matches!(err, PlatformSpecError::GlibcOnNonLinux));
    }

    #[test]
    fn defaults_glibc_to_2_28_on_linux() {
        let spec =
            PlatformSpec::new(PlatformOs::Linux, Arch::Aarch64, None, PyImpl::CPython).unwrap();
        assert_eq!(spec.glibc, Some((2, 28)));
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
        let win_x64 = PlatformSpec {
            os: PlatformOs::Windows,
            arch: Arch::X86_64,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            win_x64.to_target_triple(),
            Ok(TargetTriple::X8664PcWindowsMsvc),
        );
        let win_aarch64 = PlatformSpec {
            os: PlatformOs::Windows,
            arch: Arch::Aarch64,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            win_aarch64.to_target_triple(),
            Ok(TargetTriple::Aarch64PcWindowsMsvc),
        );
        let win_x86 = PlatformSpec {
            os: PlatformOs::Windows,
            arch: Arch::X86,
            glibc: None,
            implementation: PyImpl::CPython,
        };
        assert_eq!(
            win_x86.to_target_triple(),
            Ok(TargetTriple::I686PcWindowsMsvc),
        );
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
            Err(PlatformSpecError::UnsupportedGlibc { major: 2, .. })
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
            Err(PlatformSpecError::UnsupportedGlibc { major: 3, minor: 0, .. })
        ));
    }

    #[test]
    fn unsupported_glibc_error_message_includes_major_and_minor() {
        let spec = PlatformSpec {
            os: PlatformOs::Linux,
            arch: Arch::X86_64,
            glibc: Some((3, 0)),
            implementation: PyImpl::CPython,
        };
        let err = spec.to_target_triple().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("glibc 3.0"), "got: {message}");
        assert!(message.contains("x86_64"), "got: {message}");
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
}

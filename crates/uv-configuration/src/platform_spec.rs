//! User-facing CLI value types for `uv download --platform/--machine/--glibc/--implementation`.
//!
//! These are kept in `uv-configuration` so `uv-cli` can reference the parsers as `clap`
//! `value_parser` functions. The composition logic that maps these into a resolver
//! `TargetTriple` lives in the `uv` crate — it has no CLI-layer consumer.

use std::fmt;
use std::str::FromStr;

use uv_platform_tags::Arch;

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn implementation_from_str_rejects_non_cpython() {
        assert!("pypy".parse::<PyImpl>().is_err());
    }
}

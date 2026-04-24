use anyhow::Result;
use assert_cmd::prelude::*;
use assert_fs::prelude::*;
use sha2::{Digest, Sha256};

use uv_test::uv_snapshot;

#[test]
fn download_basic_native_platform() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);

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

    uv_snapshot!(context.filters(), context.download().arg("-o").arg(out.path()), @r"
    success: true
    exit_code: 0
    ----- stdout -----

    ----- stderr -----
    Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
    Resolved 2 packages in [TIME]
    Downloaded 1 package (0 skipped) to [TEMP_DIR]/pkgs
    ");

    // The wheel should have been materialized.
    let entries: Vec<String> = fs_err::read_dir(out.path())?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().into_string().unwrap_or_default())
        .collect();
    assert!(
        entries.iter().any(|name| {
            name.starts_with("iniconfig-")
                && std::path::Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        }),
        "expected an iniconfig wheel in {:?}, got {:?}",
        out.path(),
        entries,
    );

    // No venv should have been created under the project.
    assert!(!context.temp_dir.child(".venv").exists());

    Ok(())
}

#[test]
fn download_input_normalization_uppercase_windows_amd64() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
    context.temp_dir.child("pyproject.toml").write_str(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.12"
        dependencies = ["iniconfig"]

        [tool.uv]
        environments = [
            "sys_platform == 'win32' and platform_machine == 'x86_64' and platform_python_implementation == 'CPython'",
        ]
        "#,
    )?;

    let out_a = context.temp_dir.child("a");
    context
        .download()
        .arg("--platform")
        .arg("Windows")
        .arg("--machine")
        .arg("AMD64")
        .arg("-o")
        .arg(out_a.path())
        .assert()
        .success();

    let out_b = context.temp_dir.child("b");
    context
        .download()
        .arg("--platform")
        .arg("win32")
        .arg("--machine")
        .arg("amd64")
        .arg("-o")
        .arg(out_b.path())
        .assert()
        .success();

    let collect_names = |path: &std::path::Path| -> Vec<String> {
        let mut names: Vec<String> = fs_err::read_dir(path)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().into_string().unwrap_or_default())
            .collect();
        names.sort();
        names
    };

    assert_eq!(collect_names(out_a.path()), collect_names(out_b.path()));
    Ok(())
}

#[test]
fn download_glibc_on_non_linux_errors() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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
            .arg("--platform")
            .arg("windows")
            .arg("--glibc")
            .arg("2.28")
            .arg("-o")
            .arg(context.temp_dir.child("out").path()),
        @r"
        success: false
        exit_code: 2
        ----- stdout -----

        ----- stderr -----
        error: --glibc is only valid with --platform=linux
        "
    );
    Ok(())
}

#[test]
fn download_implementation_non_cpython_errors() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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
            .arg("--implementation")
            .arg("PyPy")
            .arg("-o")
            .arg(context.temp_dir.child("out").path()),
        @r"
        success: false
        exit_code: 2
        ----- stdout -----

        ----- stderr -----
        error: invalid value 'PyPy' for '--implementation <IMPLEMENTATION>': unsupported Python implementation `pypy`; only `CPython` is supported

        For more information, try '--help'.
        "
    );
    Ok(())
}

#[test]
fn download_missing_output_dir() {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
    uv_snapshot!(
        context.filters(),
        context.download(),
        @r"
        success: false
        exit_code: 2
        ----- stdout -----

        ----- stderr -----
        error: the following required arguments were not provided:
          --output-dir <OUTPUT_DIR>

        Usage: uv download --output-dir <OUTPUT_DIR> --cache-dir [CACHE_DIR] --exclude-newer <EXCLUDE_NEWER>

        For more information, try '--help'.
        "
    );
}

#[test]
fn download_linux_aarch64_manylinux_2_28() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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

    context
        .download()
        .arg("--platform")
        .arg("linux")
        .arg("--machine")
        .arg("aarch64")
        .arg("--glibc")
        .arg("2.28")
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();

    let has_aarch64 = fs_err::read_dir(out.path())?.any(|entry| {
        entry
            .ok()
            .map(|e| e.file_name().to_string_lossy().contains("aarch64"))
            .unwrap_or(false)
    });
    assert!(has_aarch64, "expected an aarch64 wheel in {:?}", out.path());
    Ok(())
}

#[test]
fn download_reruns_are_idempotent() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();

    // Second run should report 0 written and 1 skipped.
    uv_snapshot!(
        context.filters(),
        context.download().arg("-o").arg(out.path()),
        @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
        Resolved 2 packages in [TIME]
        Downloaded 0 packages (1 skipped) to [TEMP_DIR]/pkgs
        "
    );
    Ok(())
}

#[test]
fn download_workspace_member_skipped_with_warning() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);

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

    context.temp_dir.child("child/pyproject.toml").write_str(
        r#"
        [project]
        name = "child"
        version = "0.1.0"
        requires-python = ">=3.12"

        [build-system]
        requires = ["hatchling"]
        build-backend = "hatchling.build"
        "#,
    )?;

    let out = context.temp_dir.child("pkgs");

    let assert = context
        .download()
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("Skipping local/editable source `child`"),
        "expected `child` skip warning, got stderr:\n{stderr}"
    );

    let mut entries: Vec<String> = fs_err::read_dir(out.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().into_string().unwrap_or_default())
        .collect();
    entries.sort();
    assert!(
        !entries.iter().any(|name| name.starts_with("child-")),
        "child wheel should NOT have been downloaded: {entries:?}"
    );
    assert!(
        entries.iter().any(|name| name.starts_with("iniconfig-")),
        "iniconfig wheel missing: {entries:?}"
    );
    Ok(())
}

#[test]
fn download_locked_fails_on_mismatch() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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

    // Generate the initial lockfile.
    context.lock().assert().success();

    // Mutate the requirements.
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
        @r"
        success: false
        exit_code: 1
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
        Resolved 2 packages in [TIME]
        The lockfile at `uv.lock` needs to be updated, but `--locked` was provided. To update the lockfile, run `uv lock`.
        "
    );
    Ok(())
}

#[test]
fn download_platform_not_in_environments() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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
        @r"
        success: false
        exit_code: 2
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
        Resolved 2 packages in [TIME]
        error: target platform not listed in `tool.uv.environments`; add this environment to `tool.uv.environments` to support cross-platform downloads
        "
    );
    Ok(())
}

#[test]
fn download_wheel_hash_matches_lockfile() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();

    // Read the recorded hash from uv.lock — look for the wheel entry (contains ".whl").
    let lock_contents = fs_err::read_to_string(context.temp_dir.child("uv.lock").path())?;
    let recorded_sha = lock_contents
        .lines()
        .filter(|line| line.contains(".whl"))
        .find_map(|line| {
            line.split("hash = \"sha256:")
                .nth(1)
                .and_then(|tail| tail.split('"').next().map(str::to_owned))
        })
        .ok_or_else(|| anyhow::anyhow!("failed to find wheel sha256 in uv.lock:\n{lock_contents}"))?;

    // Find the iniconfig wheel in out-dir and hash it.
    let wheel = fs_err::read_dir(out.path())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("iniconfig-")
                        && std::path::Path::new(name)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
                })
        })
        .ok_or_else(|| anyhow::anyhow!("no iniconfig wheel in {:?}", out.path()))?;

    let bytes = fs_err::read(&wheel)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = format!("{:x}", hasher.finalize());

    assert_eq!(actual, recorded_sha, "downloaded wheel sha must match uv.lock");
    Ok(())
}

#[test]
fn download_no_binary_produces_sdist_only() -> Result<()> {
    let context = uv_test::test_context_with_versions!(&["3.12"]);
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
        .arg("--no-binary")
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();

    let names: Vec<String> = fs_err::read_dir(out.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().into_string().unwrap_or_default())
        .collect();

    assert!(
        names.iter().any(|n| {
            n.starts_with("iniconfig-")
                && (n.ends_with(".tar.gz")
                    || std::path::Path::new(n)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip")))
        }),
        "expected iniconfig sdist in {names:?}",
    );
    assert!(
        !names.iter().any(|n| {
            n.starts_with("iniconfig-")
                && std::path::Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        }),
        "no .whl should be produced when --no-binary=:all: is set, got {names:?}",
    );
    Ok(())
}

#[tokio::test]
async fn download_direct_url_wheel_hash_matches_lockfile() -> Result<()> {
    // Direct URL dependencies have their hashes recorded only in the lock, not on the
    // registry File. This test exercises the path where `resolution.hashes()` supplies
    // the per-dist hashes for a DirectUrl wheel.
    //
    // We serve a local wheel fixture through wiremock so the test stays hermetic; a
    // prior iteration hit files.pythonhosted.org directly, which was a source of
    // flakiness and an external dependency.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let context = uv_test::test_context_with_versions!(&["3.13"]);
    let server = MockServer::start().await;

    let wheel_bytes = fs_err::read(
        context
            .workspace_root
            .join("test/links/basic_package-0.1.0-py3-none-any.whl"),
    )?;
    let wheel_sha = {
        let mut hasher = Sha256::new();
        hasher.update(&wheel_bytes);
        format!("{:x}", hasher.finalize())
    };

    let wheel_url = format!(
        "{}/files/basic_package-0.1.0-py3-none-any.whl",
        server.uri()
    );

    Mock::given(method("GET"))
        .and(path("/files/basic_package-0.1.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel_bytes))
        .mount(&server)
        .await;

    context.temp_dir.child("pyproject.toml").write_str(&format!(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.13"
        dependencies = ["basic-package @ {wheel_url}#sha256={wheel_sha}"]
        "#,
    ))?;

    let out = context.temp_dir.child("pkgs");
    context
        .download()
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();

    let wheel = fs_err::read_dir(out.path())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("basic_package-"))
        })
        .ok_or_else(|| anyhow::anyhow!("no basic_package wheel in {:?}", out.path()))?;

    let bytes = fs_err::read(&wheel)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = format!("{:x}", hasher.finalize());

    assert_eq!(
        actual, wheel_sha,
        "direct-URL wheel bytes must match the SHA advertised in the `url` fragment",
    );
    Ok(())
}

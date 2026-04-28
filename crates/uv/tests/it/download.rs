use anyhow::Result;
use assert_cmd::prelude::*;
use assert_fs::prelude::*;
use sha2::{Digest, Sha256};

use uv_static::EnvVars;
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
    Downloaded 1 package (0 already existed) to [TEMP_DIR]/pkgs
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

#[tokio::test]
async fn download_reports_starting_downloads() -> Result<()> {
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

    let pyproject_toml = context.temp_dir.child("pyproject.toml");
    pyproject_toml.write_str(&format!(
        r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.13"
        dependencies = ["basic-package @ {wheel_url}#sha256={wheel_sha}"]
        "#,
    ))?;

    let out = context.temp_dir.child("pkgs");

    uv_snapshot!(
        context.filters(),
        context
            .download()
            .env_remove(EnvVars::UV_TEST_NO_CLI_PROGRESS)
            .arg("-o")
            .arg(out.path()),
        @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.13.[X] interpreter at: [PYTHON-3.13]
        Resolved 2 packages in [TIME]
        Starting downloads...
        Downloaded 1 package (0 already existed) to [TEMP_DIR]/pkgs
        "
    );

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

    // Second run should report 0 written and 1 already-existed.
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
        Downloaded 0 packages (1 already existed) to [TEMP_DIR]/pkgs
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

#[test]
fn download_default_index_local_path_warns() -> Result<()> {
    // `--default-index` pointing at a local path cannot be used as a mirror for rewriting
    // `files.pythonhosted.org` URLs, so uv download should warn and fall back to the URLs
    // recorded in the lockfile. Uses `--frozen` so the resolve step doesn't consult the
    // (empty) path index.
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

    // Generate the lockfile against PyPI first, so `uv.lock` has real URLs.
    context.lock().assert().success();

    // Point `--default-index` at an empty local directory. The directory must exist
    // because `IndexUrl::parse` resolves the path.
    let fake_index = context.temp_dir.child("fake_index");
    fake_index.create_dir_all()?;
    let fake_index_url = format!("file://{}", fake_index.path().display());

    let out = context.temp_dir.child("pkgs");
    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--frozen")
            .arg("--default-index")
            .arg(&fake_index_url)
            .arg("-o").arg(out.path()),
        @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
        warning: `--default-index` points at a local path; `uv download` cannot rewrite recorded artifact URLs to a filesystem index and will use the URLs in `uv.lock` as-is
        Downloaded 1 package (0 already existed) to [TEMP_DIR]/pkgs
        "
    );
    Ok(())
}

#[test]
fn download_trusts_existing_wheel_without_rehashing() -> Result<()> {
    // The atomic partial→rename pattern in `download_to` and `copy_or_link` guarantees that
    // a regular file at `dst` is whole and already passed hash verification on its first
    // download — re-hashing on every rerun is pure I/O cost for no information gain. This
    // test documents that decision: a tampered file is *not* re-validated and is left
    // untouched. Users who need to detect drift in an existing `--output-dir` should
    // either delete the file to force re-download or use a dedicated checker.
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

    // Tamper with the iniconfig wheel in place.
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
    let tampered = b"not a real wheel";
    fs_err::write(&wheel, tampered)?;

    uv_snapshot!(
        context.filters(),
        context.download().arg("--frozen").arg("-o").arg(out.path()),
        @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
        Downloaded 0 packages (1 already existed) to [TEMP_DIR]/pkgs
        "
    );

    // The tampered bytes are still on disk — the rerun did not overwrite them.
    let after = fs_err::read(&wheel)?;
    assert_eq!(
        after, tampered,
        "tampered wheel was unexpectedly overwritten"
    );
    Ok(())
}

#[test]
fn download_rejects_unknown_extra() -> Result<()> {
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

    let assert = context
        .download()
        .arg("--extra")
        .arg("nope")
        .arg("-o")
        .arg(context.temp_dir.child("pkgs").path())
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("nope")
            && stderr.to_ascii_lowercase().contains("extra"),
        "expected unknown-extra error, got stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn download_rejects_unknown_group() -> Result<()> {
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

    let assert = context
        .download()
        .arg("--group")
        .arg("nope")
        .arg("-o")
        .arg(context.temp_dir.child("pkgs").path())
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("nope")
            && (stderr.to_ascii_lowercase().contains("group")
                || stderr.to_ascii_lowercase().contains("dependency-group")),
        "expected unknown-group error, got stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn download_explicit_pypi_simple_with_trailing_slash() -> Result<()> {
    // Previously `--default-index https://pypi.org/simple/` (with trailing slash) was not
    // tagged as `IndexUrl::Pypi` and would rewrite artifact URLs to the invalid
    // `https://pypi.org/packages/...`. Treating it as PyPI itself keeps the real
    // `files.pythonhosted.org` URLs in place.
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
    context.lock().assert().success();

    let out = context.temp_dir.child("pkgs");
    let assert = context
        .download()
        .arg("--frozen")
        .arg("--default-index")
        .arg("https://pypi.org/simple/")
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("will use the URLs in `uv.lock` as-is"),
        "PyPI should not trigger the mirror-fallback warn path, got:\n{stderr}"
    );
    Ok(())
}

#[test]
fn download_default_index_non_simple_url_warns() -> Result<()> {
    // `--default-index` pointing at a URL whose final segment is not `simple` / `+simple`
    // can't be turned into a mirror base, so uv download should warn and fall back to the
    // URLs recorded in the lockfile.
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

    context.lock().assert().success();

    let out = context.temp_dir.child("pkgs");
    uv_snapshot!(
        context.filters(),
        context
            .download()
            .arg("--frozen")
            .arg("--default-index")
            .arg("https://example.invalid/pypi/not-ending-in-simple")
            .arg("-o").arg(out.path()),
        @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        Using CPython 3.12.[X] interpreter at: [PYTHON-3.12]
        warning: `--default-index` was provided but its URL does not end in `simple` / `+simple`; `uv download` does not know how to derive a mirror file base and will use the URLs in `uv.lock` as-is
        Downloaded 1 package (0 already existed) to [TEMP_DIR]/pkgs
        "
    );
    Ok(())
}

#[test]
fn download_default_index_trailing_slash_does_not_force_resolve() -> Result<()> {
    // `IndexUrl`'s `Hash`/`Eq` are derived from raw URL bytes, and `Lock::satisfies`
    // builds its remote-index set with `UrlString::from(url)` which is also byte-wise
    // (see `crates/uv-resolver/src/lock/mod.rs` around line 1925). So lockfile-source
    // `https://x/simple/` paired with CLI `https://x/simple` would otherwise produce
    // `MissingRemoteIndex` and force a fresh resolve — which on a flaky mirror can
    // 302→403 on macOS-only wheels that don't exist locally.
    //
    // With alignment, the CLI form is rewritten to match the lockfile, the lock
    // satisfies on the first try, and `--locked` mode (which would surface a real
    // mismatch as a hard error) succeeds.
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
    // Lock with the trailing-slash form so the lockfile records `.../simple/`.
    context
        .lock()
        .arg("--default-index")
        .arg("https://pypi.org/simple/")
        .assert()
        .success();

    let out = context.temp_dir.child("pkgs");
    // Pass the no-slash form. `--locked` keeps the lock file untouched: if alignment
    // failed and the lock no longer satisfied, the resolver would re-resolve and then
    // detect a mismatch against the on-disk lock, exiting non-zero — exactly the
    // signal we want to fail loudly on without depending on stderr line shape.
    context
        .download()
        .arg("--locked")
        .arg("--default-index")
        .arg("https://pypi.org/simple")
        .arg("-o")
        .arg(out.path())
        .assert()
        .success();
    Ok(())
}

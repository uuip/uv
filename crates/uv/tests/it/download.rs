use anyhow::Result;
use assert_cmd::prelude::*;
use assert_fs::prelude::*;

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
    warning: Skipping local/editable source `project` (not materialized into --output-dir)
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
        warning: Skipping local/editable source `project` (not materialized into --output-dir)
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

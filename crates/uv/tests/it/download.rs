use anyhow::Result;
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

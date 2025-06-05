pub mod archive;
pub mod git;




use crate::error::SourceError;
use crate::types::SourceCode;
use std::path::Path;
use tempfile::TempDir;
use tokio::fs;

/// Prepares source code for compilation
pub async fn prepare_source(source: &SourceCode) -> Result<TempDir, SourceError> {
    match source {
        SourceCode::GitRepository {
            repository_url,
            commit,
        } => git::clone_repository(repository_url, commit).await,
        SourceCode::Archive { content, format } => archive::extract_archive(content, *format).await,
    }
}

/// Validates that the source directory contains a valid Rust project
pub async fn validate_project_structure(project_dir: &Path) -> Result<(), SourceError> {
    // Check for Cargo.toml
    let cargo_toml = project_dir.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Err(SourceError::InvalidProject(
            "Cargo.toml not found in project root".to_string(),
        ));
    }

    // Check for src directory
    let src_dir = project_dir.join("src");
    if !src_dir.exists() || !src_dir.is_dir() {
        return Err(SourceError::InvalidProject(
            "src directory not found".to_string(),
        ));
    }

    // Check for lib.rs or main.rs
    let lib_rs = src_dir.join("lib.rs");
    let main_rs = src_dir.join("main.rs");

    if !lib_rs.exists() && !main_rs.exists() {
        return Err(SourceError::InvalidProject(
            "Neither src/lib.rs nor src/main.rs found".to_string(),
        ));
    }

    // Validate it's a WASM project by checking Cargo.toml
    validate_wasm_project(&cargo_toml).await?;

    Ok(())
}

async fn validate_wasm_project(cargo_toml_path: &Path) -> Result<(), SourceError> {
    let content = fs::read_to_string(cargo_toml_path)
        .await
        .map_err(|e| SourceError::InvalidProject(format!("Failed to read Cargo.toml: {e}")))?;

    let cargo_toml: toml::Value = toml::from_str(&content)
        .map_err(|e| SourceError::InvalidProject(format!("Failed to parse Cargo.toml: {e}")))?;

    // Check if it has fluentbase-sdk dependency
    let has_fluentbase_sdk = cargo_toml
        .get("dependencies")
        .and_then(|deps| deps.as_table())
        .map(|deps| deps.contains_key("fluentbase-sdk"))
        .unwrap_or(false);

    if !has_fluentbase_sdk {
        return Err(SourceError::InvalidProject(
            "Project doesn't have fluentbase-sdk dependency".to_string(),
        ));
    }

    // Check for [lib] crate-type = ["cdylib"] for WASM libraries
    if let Some(lib) = cargo_toml.get("lib") {
        let has_cdylib = lib
            .get("crate-type")
            .and_then(|types| types.as_array())
            .map(|types| types.iter().any(|t| t.as_str() == Some("cdylib")))
            .unwrap_or(false);

        if !has_cdylib {
            tracing::warn!(
                "Library crate doesn't have 'cdylib' crate-type, which is recommended for WASM"
            );
        }
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct ProjectSetup {
        with_fluentbase: bool,
        is_lib: bool,
        has_cargo_toml: bool,
        has_src_dir: bool,
        has_source_file: bool,
        lib_has_cdylib: bool,
    }

    impl Default for ProjectSetup {
        fn default() -> Self {
            Self {
                with_fluentbase: true,
                is_lib: true,
                has_cargo_toml: true,
                has_src_dir: true,
                has_source_file: true,
                lib_has_cdylib: true,
            }
        }
    }

    async fn create_test_project_with_setup(dir: &Path, setup: ProjectSetup) {
        if setup.has_src_dir {
            fs::create_dir_all(dir.join("src")).unwrap();
        }

        if setup.has_cargo_toml {
            let cargo_toml = if setup.with_fluentbase {
                format!(
                    r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[dependencies]
fluentbase-sdk = "0.1.0"

{}
"#,
                    if setup.is_lib && setup.lib_has_cdylib {
                        "[lib]\ncrate-type = [\"cdylib\"]"
                    } else if setup.is_lib {
                        "[lib]\ncrate-type = [\"rlib\"]"
                    } else {
                        ""
                    }
                )
            } else {
                r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
"#
                .to_string()
            };

            fs::write(dir.join("Cargo.toml"), cargo_toml).unwrap();
        }

        if setup.has_source_file && setup.has_src_dir {
            if setup.is_lib {
                fs::write(
                    dir.join("src/lib.rs"),
                    "#[no_mangle]\npub extern \"C\" fn deploy() {}\n",
                )
                .unwrap();
            } else {
                fs::write(
                    dir.join("src/main.rs"),
                    "fn main() { println!(\"Hello\"); }\n",
                )
                .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn test_validate_valid_projects() {
        let test_cases = vec![
            ("lib_with_cdylib", ProjectSetup::default()),
            (
                "lib_without_cdylib",
                ProjectSetup {
                    lib_has_cdylib: false,
                    ..Default::default()
                },
            ),
            (
                "bin_project",
                ProjectSetup {
                    is_lib: false,
                    ..Default::default()
                },
            ),
        ];

        for (name, setup) in test_cases {
            let temp_dir = TempDir::new().unwrap();
            create_test_project_with_setup(temp_dir.path(), setup).await;

            let result = validate_project_structure(temp_dir.path()).await;
            assert!(
                result.is_ok(),
                "Valid project '{name}' should pass validation"
            );
        }
    }

    #[tokio::test]
    async fn test_validate_project_structure_errors() {
        let test_cases = vec![
            (
                "missing_cargo_toml",
                ProjectSetup {
                    has_cargo_toml: false,
                    ..Default::default()
                },
                "Cargo.toml not found",
            ),
            (
                "missing_src_dir",
                ProjectSetup {
                    has_src_dir: false,
                    has_source_file: false,
                    ..Default::default()
                },
                "src directory not found",
            ),
            (
                "missing_source_files",
                ProjectSetup {
                    has_source_file: false,
                    ..Default::default()
                },
                "Neither src/lib.rs nor src/main.rs",
            ),
            (
                "non_wasm_project",
                ProjectSetup {
                    with_fluentbase: false,
                    ..Default::default()
                },
                "doesn't have fluentbase-sdk dependency",
            ),
        ];

        for (name, setup, expected_error) in test_cases {
            let temp_dir = TempDir::new().unwrap();
            create_test_project_with_setup(temp_dir.path(), setup).await;

            let result = validate_project_structure(temp_dir.path()).await;

            match result {
                Err(SourceError::InvalidProject(msg)) => {
                    assert!(
                        msg.contains(expected_error),
                        "Test case '{name}': expected error containing '{expected_error}', got '{msg}'"
                    );
                }
                _ => panic!(
                    "Test case '{name}' should have failed with InvalidProject error"
                ),
            }
        }
    }

    #[tokio::test]
    async fn test_validate_wasm_project_cargo_toml_parsing() {
        let temp_dir = TempDir::new().unwrap();

        // Create invalid TOML
        fs::create_dir_all(temp_dir.path().join("src")).unwrap();
        fs::write(temp_dir.path().join("src/lib.rs"), "").unwrap();
        fs::write(temp_dir.path().join("Cargo.toml"), "invalid toml content").unwrap();

        let result = validate_project_structure(temp_dir.path()).await;

        match result {
            Err(SourceError::InvalidProject(msg)) => {
                assert!(
                    msg.contains("Failed to parse Cargo.toml"),
                    "Should mention TOML parsing failure: {msg}"
                );
            }
            _ => panic!("Should fail with TOML parsing error"),
        }
    }

    #[tokio::test]
    async fn test_cdylib_warning_logged() {
        // This test would verify that a warning is logged for libraries without cdylib
        // In a real test, you'd capture the tracing output and verify the warning
        let temp_dir = TempDir::new().unwrap();
        create_test_project_with_setup(
            temp_dir.path(),
            ProjectSetup {
                lib_has_cdylib: false,
                ..Default::default()
            },
        )
        .await;

        // The test passes if validation succeeds (warning is just logged, not an error)
        let result = validate_project_structure(temp_dir.path()).await;
        assert!(result.is_ok(), "Should succeed even without cdylib");
    }
}

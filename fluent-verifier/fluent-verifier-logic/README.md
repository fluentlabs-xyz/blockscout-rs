# Fluent Verifier Logic

**fluent-verifier-logic** is a Rust crate providing core functionality to verify Fluent Wasm smart contracts. It
orchestrates fetching source code, compiling it in a reproducible Docker environment, and comparing the resulting
bytecode against a deployed contract's hash.

The crate is primarily composed of:

* **Contract Verifier:** Orchestrates the end-to-end verification.
* **Docker Runner:** Manages Dockerized compilation.
* **Source Handler:** Prepares source code from Git or archives.

## Contract Verifier (`verifier.rs`)

**Purpose:** Orchestrates the complete verification workflow for Fluent Wasm smart contracts, ensuring deployed
contracts match their claimed source code.

**Key Features:**

* **End-to-End Pipeline:** Manages source acquisition, Dockerized compilation, and bytecode comparison.
* **Multi-Source Support:** Handles Git repositories (HTTPS) and archives (`.tar.gz`, `.zip`).
* **Rustc Version Resolution:** Automatically determines `rustc` version (user input > `rust-toolchain.toml` > default).
* **Reproducible Builds & Artifact Collection:** Uses Docker for consistent builds and gathers all relevant artifacts.
* **Resource Management:** Automatically cleans up temporary directories.

**Basic Usage (`verify_contract`):**

```rust
use fluent_verifier_logic::{
    verify_contract, connect_docker, ArchiveFormat, BuildProfile, CompileSettings,
    SourceCode, VerificationError, VerificationSettings, VerifyRequest,
};
use semver::Version;
use bytes::Bytes; // For archive content

async fn run_verification_example() -> anyhow::Result<()> {
    let docker_url = "unix:///var/run/docker.sock";
    let docker = connect_docker(docker_url).await?;

    let verification_request = VerifyRequest {
        source: SourceCode::GitRepository { // Or SourceCode::Archive
            repository_url: "https://github.com/your-org/your-contract.git".parse()?,
            commit: "main".to_string(),
        },
        deployed_bytecode_hash: "expected_rwasm_hash".to_string(),
        compile_settings: CompileSettings {
            rustc_version: None,
            fluentbase_sdk_version: Version::parse("0.1.0")?,
            build_profile: BuildProfile::Release,
            features: vec![], contract_name: None, cargo_flags: vec![],
        },
        default_settings: VerificationSettings {
            default_rustc_version: Version::parse("1.75.0")?,
            docker_url: docker_url.to_string(),
            docker_image_prefix: "your_docker_org/fluent".to_string(),
            compile_timeout_secs: 300,
        },
    };

    match verify_contract(&docker, verification_request).await {
        Ok(success) => println!("✅ Contract Verified: {}", success.contract_name),
        Err(e) => eprintln!("❌ Verification Failed: {}", e),
    }
    Ok(())
}
```

**Core Configuration / Expectations:**

* Project must be a valid Rust project (`Cargo.toml`, `src/lib.rs` or `src/main.rs`).
* `Cargo.toml` must include `fluentbase-sdk` dependency.
* `rust-toolchain.toml` (optional) for specific `rustc` version.

## Docker Runner (`docker_runner.rs`)

**Purpose:** Executes the `fluent-wasm-compiler-cli` within isolated Docker containers for reproducible builds.

**Key Features:**

* **Automated Image Management:** Builds/caches Docker images tagged with `rustc` versions, installing
  `fluent-wasm-compiler-cli`.
* **Isolated & Auto-Cleaned Compilations:** New container per run, auto-removed on exit.
* **Timeout Protection & Artifact Retrieval:** Enforces time limits and extracts build artifacts/logs.

**Usage Note:** Primarily used internally by the `ContractVerifier`. The main function is `run_compiler(...)` which
takes Docker client, source path, compile settings, resolved versions, image prefix, and timeout.

**Core Configuration / Expectations:**

* Docker images: `{docker_image_prefix}/fluent-compiler:rust-{rustc_version}`.
* `fluent-wasm-compiler-cli` in image must output JSON with artifact paths and metadata.

## Source Handler (`source/`)

**Purpose:** Prepares smart contract source code from Git or archives for compilation and validates project structure.

**Key Features:**

* **Git Integration:** Clones HTTPS repositories and checks out specific commits.
* **Archive Extraction:** Supports `.tar.gz` and `.zip`.
* **Structure Normalization & Validation:** Attempts to fix single-directory archive nesting and validates project for
  `Cargo.toml`, `src` files, and `fluentbase-sdk` dependency.

**Usage Note:** Primarily used internally by the `ContractVerifier`. Key functions are `prepare_source(...)` for
fetching/extracting and `validate_project_structure(...)` for checks.

## Public API

The main public interface is exposed through `fluent_verifier_logic::lib.rs`, re-exporting:

* `verify_contract`: The primary function for end-to-end verification.
* `connect_docker`: A helper to establish a connection with a Docker daemon.
* Various types (e.g., `VerifyRequest`, `CompileSettings`, `SourceCode`) and error enums.

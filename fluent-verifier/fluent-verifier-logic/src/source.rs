use crate::{
    error::VerificationError,
    proto::{ArchiveSource, GitSource},
};
use flate2::read::GzDecoder;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use tar::Archive;
use tempfile::TempDir;
use tracing::info;
use zip::ZipArchive;

/// Prepares source code from archive
pub async fn prepare_source_from_archive(
    temp_dir: &TempDir,
    archive: &ArchiveSource,
) -> Result<PathBuf, VerificationError> {
    let content = &archive.content;
    let format = detect_archive_format(content)?;

    extract_archive(temp_dir, content, format).await?;

    Ok(temp_dir.path().to_path_buf())
}

/// Prepares source code from git repository
pub async fn prepare_source_from_git(
    temp_dir: &TempDir,
    git: &GitSource,
) -> Result<PathBuf, VerificationError> {
    use url::Url;

    // Validate URL
    let url = Url::parse(&git.repository_url)
        .map_err(|e| VerificationError::InvalidRequest(format!("Invalid git URL: {e}")))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(VerificationError::InvalidRequest(
            "Only HTTPS/HTTP git URLs are supported".to_string(),
        ));
    }

    // Try simple clone first (works for public repos)
    let mut prep = gix::prepare_clone(git.repository_url.as_str(), temp_dir.path())?
        .with_ref_name(if !git.commit_ref.is_empty() {
            Some(git.commit_ref.as_str())
        } else {
            None
        })
        .unwrap();

    let should_interrupt = AtomicBool::new(false);

    let (mut checkout, _) = prep.fetch_then_checkout(gix::progress::Discard, &should_interrupt)?;
    let (repo, _) = checkout.main_worktree(gix::progress::Discard, &should_interrupt)?;

    info!(
        "Checkout repo into: {} hash={:?}",
        repo.workdir().unwrap().display(),
        repo.head()?.try_peel_to_id()?
    );

    Ok(temp_dir.path().to_path_buf())
}

/// Collects all source files from directory
pub async fn collect_source_files(
    dir: PathBuf,
) -> Result<std::collections::BTreeMap<String, String>, VerificationError> {
    use walkdir::WalkDir;

    let mut files = std::collections::BTreeMap::new();

    for entry in WalkDir::new(&dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
    {
        let path = entry.path();

        // Skip unnecessary directories
        if path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("target") | Some(".git") | Some("node_modules")
            )
        }) {
            continue;
        }

        // Include only source files
        let include = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| matches!(ext, "rs" | "toml" | "lock" | "json" | "md"))
            .unwrap_or(false);

        if include {
            let relative = path
                .strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();

            let content = tokio::fs::read_to_string(path).await?;
            files.insert(relative, content);
        }
    }

    Ok(files)
}

// Archive format detection and extraction

#[derive(Debug, Clone, Copy)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

fn detect_archive_format(content: &[u8]) -> Result<ArchiveFormat, VerificationError> {
    if content.len() >= 2 && content[0] == 0x1f && content[1] == 0x8b {
        Ok(ArchiveFormat::TarGz)
    } else if content.len() >= 4 && &content[0..4] == b"PK\x03\x04" {
        Ok(ArchiveFormat::Zip)
    } else {
        Err(VerificationError::Source(
            "Unknown archive format".to_string(),
        ))
    }
}

async fn extract_archive(
    temp_dir: &TempDir,
    content: &[u8],
    format: ArchiveFormat,
) -> Result<(), VerificationError> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(content, &temp_dir).await?,
        ArchiveFormat::Zip => extract_zip(content, &temp_dir).await?,
    }
    normalize_archive_structure(&temp_dir).await?;
    Ok(())
}

async fn extract_tar_gz(content: &[u8], temp_dir: &TempDir) -> Result<(), VerificationError> {
    let decoder = GzDecoder::new(content);
    let mut archive = Archive::new(decoder);

    archive
        .unpack(temp_dir.path())
        .map_err(|e| VerificationError::Source(format!("Extraction failed: {e}")))?;

    Ok(())
}

async fn extract_zip(content: &[u8], temp_dir: &TempDir) -> Result<(), VerificationError> {
    let reader = std::io::Cursor::new(content);
    let mut zip = ZipArchive::new(reader)
        .map_err(|e| VerificationError::Source(format!("Failed to read ZIP archive: {e}")))?;

    for i in 0..zip.len() {
        let mut file = zip
            .by_index(i)
            .map_err(|e| VerificationError::Source(format!("Failed to read ZIP entry: {e}")))?;

        let path = file.mangled_name();
        let dest_path = temp_dir.path().join(&path);

        if file.is_dir() {
            std::fs::create_dir_all(&dest_path).map_err(|e| {
                VerificationError::Source(format!("Failed to create directory: {e}"))
            })?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    VerificationError::Source(format!("Failed to create parent directory: {e}"))
                })?;
            }

            let mut dest_file = std::fs::File::create(&dest_path)
                .map_err(|e| VerificationError::Source(format!("Failed to create file: {e}")))?;

            std::io::copy(&mut file, &mut dest_file)
                .map_err(|e| VerificationError::Source(format!("Failed to extract file: {e}")))?;
        }
    }

    Ok(())
}

async fn normalize_archive_structure(temp_dir: &TempDir) -> Result<(), VerificationError> {
    let entries: Vec<_> = std::fs::read_dir(temp_dir.path())
        .map_err(|e| VerificationError::Source(format!("Failed to read directory: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| VerificationError::Source(format!("Failed to read directory entry: {e}")))?;

    if entries.len() == 1 {
        let entry = &entries[0];
        let path = entry.path();

        if path.is_dir() && path.join("Cargo.toml").exists() {
            let temp_move_dir = tempfile::tempdir_in(temp_dir.path()).map_err(|e| {
                VerificationError::Source(format!("Failed to create temp directory: {e}"))
            })?;

            std::fs::rename(&path, temp_move_dir.path().join("content"))
                .map_err(|e| VerificationError::Source(format!("Failed to move directory: {e}")))?;

            for entry in std::fs::read_dir(temp_move_dir.path().join("content"))
                .map_err(|e| VerificationError::Source(format!("Failed to read directory: {e}")))?
            {
                let entry = entry
                    .map_err(|e| VerificationError::Source(format!("Failed to read entry: {e}")))?;
                let dest = temp_dir.path().join(entry.file_name());

                std::fs::rename(entry.path(), dest)
                    .map_err(|e| VerificationError::Source(format!("Failed to move file: {e}")))?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    use tar::Builder;

    fn create_test_tar_gz() -> Bytes {
        let mut tar_data = Vec::new();
        {
            let mut tar = Builder::new(&mut tar_data);

            let cargo_toml = b"[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n[dependencies]\nfluentbase-sdk = \"0.1.0\"";
            let mut header = tar::Header::new_gnu();
            header.set_path("test-project/Cargo.toml").unwrap();
            header.set_size(cargo_toml.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &cargo_toml[..]).unwrap();

            let mut header = tar::Header::new_gnu();
            header.set_path("test-project/src").unwrap();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::dir());
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, &mut std::io::empty()).unwrap();

            let lib_rs = b"#[no_mangle]\npub extern \"C\" fn deploy() {}";
            let mut header = tar::Header::new_gnu();
            header.set_path("test-project/src/lib.rs").unwrap();
            header.set_size(lib_rs.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &lib_rs[..]).unwrap();

            tar.finish().unwrap();
        }

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        Bytes::from(encoder.finish().unwrap())
    }

    fn create_test_zip() -> Bytes {
        use zip::write::{FileOptions, ZipWriter};

        let mut data = Vec::new();
        {
            let mut zip = ZipWriter::new(std::io::Cursor::new(&mut data));
            let options: FileOptions<()> =
                FileOptions::default().compression_method(zip::CompressionMethod::Stored);

            zip.start_file("test-project/Cargo.toml", options).unwrap();
            zip.write_all(b"[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n[dependencies]\nfluentbase-sdk = \"0.1.0\"").unwrap();

            zip.add_directory("test-project/src", options).unwrap();
            zip.start_file("test-project/src/lib.rs", options).unwrap();
            zip.write_all(b"#[no_mangle]\npub extern \"C\" fn deploy() {}")
                .unwrap();

            zip.finish().unwrap();
        }

        Bytes::from(data)
    }

    #[tokio::test]
    async fn test_detect_format() {
        let tar_gz = create_test_tar_gz();
        assert!(matches!(
            detect_archive_format(tar_gz.as_ref()),
            Ok(ArchiveFormat::TarGz)
        ));

        let zip = create_test_zip();
        assert!(matches!(
            detect_archive_format(zip.as_ref()),
            Ok(ArchiveFormat::Zip)
        ));

        let invalid = Bytes::from(b"not an archive".to_vec());
        assert!(detect_archive_format(invalid.as_ref()).is_err());
    }

    #[tokio::test]
    async fn test_extract_tar_gz() {
        let content = create_test_tar_gz();
        let temp_dir = tempfile::tempdir().unwrap();
        extract_archive(&temp_dir, &content, ArchiveFormat::TarGz)
            .await
            .unwrap();

        assert!(temp_dir.path().join("Cargo.toml").exists());
        assert!(temp_dir.path().join("src/lib.rs").exists());

        let cargo_content = std::fs::read_to_string(temp_dir.path().join("Cargo.toml")).unwrap();
        assert!(cargo_content.contains("fluentbase-sdk"));
    }

    #[tokio::test]
    async fn test_extract_zip() {
        let content = create_test_zip();
        let temp_dir = tempfile::tempdir().unwrap();
        extract_archive(&temp_dir, &content, ArchiveFormat::Zip)
            .await
            .unwrap();

        assert!(temp_dir.path().join("Cargo.toml").exists());
        assert!(temp_dir.path().join("src/lib.rs").exists());
    }
}

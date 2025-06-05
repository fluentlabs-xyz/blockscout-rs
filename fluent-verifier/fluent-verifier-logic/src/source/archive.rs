use crate::{error::SourceError, ArchiveFormat};
use bytes::Bytes;
use flate2::read::GzDecoder;
use tar::Archive;
use tempfile::TempDir;
use zip::ZipArchive;

pub async fn extract_archive(
    content: &Bytes,
    format: ArchiveFormat,
) -> Result<TempDir, SourceError> {
    let temp_dir = tempfile::tempdir()
        .map_err(|e| SourceError::ArchiveError(format!("Failed to create temp dir: {e}")))?;

    match format {
        ArchiveFormat::TarGz => extract_tar_gz(content, &temp_dir).await?,
        ArchiveFormat::Zip => extract_zip(content, &temp_dir).await?,
    }

    normalize_archive_structure(&temp_dir).await?;

    Ok(temp_dir)
}

async fn extract_tar_gz(content: &Bytes, temp_dir: &TempDir) -> Result<(), SourceError> {
    let decoder = GzDecoder::new(content.as_ref());
    let mut archive = Archive::new(decoder);

    archive
        .unpack(temp_dir.path())
        .map_err(|e| SourceError::ArchiveError(format!("Extraction failed: {e}")))?;

    Ok(())
}

async fn extract_zip(content: &Bytes, temp_dir: &TempDir) -> Result<(), SourceError> {
    let reader = std::io::Cursor::new(content);
    let mut zip = ZipArchive::new(reader)
        .map_err(|e| SourceError::ArchiveError(format!("Failed to read ZIP archive: {e}")))?;

    for i in 0..zip.len() {
        let mut file = zip
            .by_index(i)
            .map_err(|e| SourceError::ArchiveError(format!("Failed to read ZIP entry: {e}")))?;

        let path = file.mangled_name();
        let dest_path = temp_dir.path().join(&path);

        if file.is_dir() {
            std::fs::create_dir_all(&dest_path).map_err(|e| {
                SourceError::ArchiveError(format!("Failed to create directory: {e}"))
            })?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    SourceError::ArchiveError(format!("Failed to create parent directory: {e}"))
                })?;
            }

            let mut dest_file = std::fs::File::create(&dest_path)
                .map_err(|e| SourceError::ArchiveError(format!("Failed to create file: {e}")))?;

            std::io::copy(&mut file, &mut dest_file)
                .map_err(|e| SourceError::ArchiveError(format!("Failed to extract file: {e}")))?;
        }
    }

    Ok(())
}

async fn normalize_archive_structure(temp_dir: &TempDir) -> Result<(), SourceError> {
    let entries: Vec<_> = std::fs::read_dir(temp_dir.path())
        .map_err(|e| SourceError::ArchiveError(format!("Failed to read directory: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SourceError::ArchiveError(format!("Failed to read directory entry: {e}")))?;

    if entries.len() == 1 {
        let entry = &entries[0];
        let path = entry.path();

        if path.is_dir() && path.join("Cargo.toml").exists() {
            let temp_move_dir = tempfile::tempdir_in(temp_dir.path()).map_err(|e| {
                SourceError::ArchiveError(format!("Failed to create temp directory: {e}"))
            })?;

            std::fs::rename(&path, temp_move_dir.path().join("content")).map_err(|e| {
                SourceError::ArchiveError(format!("Failed to move directory: {e}"))
            })?;

            for entry in std::fs::read_dir(temp_move_dir.path().join("content")).map_err(|e| {
                SourceError::ArchiveError(format!("Failed to read directory: {e}"))
            })? {
                let entry = entry.map_err(|e| {
                    SourceError::ArchiveError(format!("Failed to read entry: {e}"))
                })?;
                let dest = temp_dir.path().join(entry.file_name());

                std::fs::rename(entry.path(), dest).map_err(|e| {
                    SourceError::ArchiveError(format!("Failed to move file: {e}"))
                })?;
            }
        }
    }

    Ok(())
}

pub fn detect_format(content: &Bytes) -> Result<ArchiveFormat, SourceError> {
    if content.len() >= 2 && content[0] == 0x1f && content[1] == 0x8b {
        Ok(ArchiveFormat::TarGz)
    } else if content.len() >= 4 && &content[0..4] == b"PK\x03\x04" {
        Ok(ArchiveFormat::Zip)
    } else {
        Err(SourceError::InvalidArchiveFormat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
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
        assert!(matches!(detect_format(&tar_gz), Ok(ArchiveFormat::TarGz)));

        let zip = create_test_zip();
        assert!(matches!(detect_format(&zip), Ok(ArchiveFormat::Zip)));

        let invalid = Bytes::from(b"not an archive".to_vec());
        assert!(matches!(
            detect_format(&invalid),
            Err(SourceError::InvalidArchiveFormat)
        ));
    }

    #[tokio::test]
    async fn test_extract_tar_gz() {
        let content = create_test_tar_gz();
        let temp_dir = extract_archive(&content, ArchiveFormat::TarGz)
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
        let temp_dir = extract_archive(&content, ArchiveFormat::Zip).await.unwrap();

        assert!(temp_dir.path().join("Cargo.toml").exists());
        assert!(temp_dir.path().join("src/lib.rs").exists());
    }

    #[tokio::test]
    async fn test_normalize_archive_structure() {
        let content = create_test_tar_gz();
        let temp_dir = extract_archive(&content, ArchiveFormat::TarGz)
            .await
            .unwrap();

        assert!(temp_dir.path().join("Cargo.toml").exists());
        assert!(!temp_dir.path().join("test-project/Cargo.toml").exists());
    }

    #[tokio::test]
    async fn test_empty_archive() {
        let mut tar_data = Vec::new();
        {
            let mut tar = Builder::new(&mut tar_data);
            tar.finish().unwrap();
        }

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        let content = Bytes::from(encoder.finish().unwrap());

        let result = extract_archive(&content, ArchiveFormat::TarGz).await;
        assert!(result.is_ok());
    }
}

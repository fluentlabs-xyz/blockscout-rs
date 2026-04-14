use cargo_metadata::{MetadataCommand, TargetKind};
use std::path::{Path, PathBuf};

pub(crate) fn cdylib_target_name(manifest_path: impl AsRef<Path>) -> anyhow::Result<String> {
    let manifest_path = manifest_path.as_ref();

    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path)
        .no_deps()
        .exec()?;

    let manifest_path = manifest_path.canonicalize()?;

    let package = metadata
        .packages
        .iter()
        .find(|pkg| {
            pkg.manifest_path
                .clone()
                .into_std_path_buf()
                .canonicalize()
                .ok()
                .as_ref()
                == Some(&manifest_path)
        })
        .ok_or_else(|| anyhow::anyhow!("package not found for {}", manifest_path.display()))?;

    let target = package
        .targets
        .iter()
        .find(|target| target.kind.iter().any(|kind| kind == &TargetKind::CDyLib))
        .ok_or_else(|| anyhow::anyhow!("no cdylib target found in {}", manifest_path.display()))?;

    Ok(target.name.clone())
}

pub(crate) fn workspace_manifest_path(source_manifest_path: &PathBuf) -> anyhow::Result<PathBuf> {
    let metadata = MetadataCommand::new()
        .manifest_path(&source_manifest_path)
        .no_deps()
        .exec()?;

    let workspace_manifest = metadata.workspace_root.join("Cargo.toml").canonicalize()?;
    if source_manifest_path != &workspace_manifest {
        Ok(workspace_manifest)
    } else {
        Ok(source_manifest_path.clone())
    }
}

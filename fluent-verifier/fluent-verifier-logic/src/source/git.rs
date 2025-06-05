use crate::error::SourceError;
use git2::Repository;
use tempfile::TempDir;
use url::Url;

pub async fn clone_repository(repository_url: &Url, commit: &str) -> Result<TempDir, SourceError> {
    validate_git_url(repository_url)?;

    let temp_dir = tempfile::tempdir()
        .map_err(|e| SourceError::GitError(format!("Failed to create temp dir: {e}")))?;

    let repo = Repository::clone(repository_url.as_str(), temp_dir.path()).map_err(|e| match e
        .code()
    {
        git2::ErrorCode::Auth => SourceError::RepositoryNotFound(repository_url.to_string()),
        git2::ErrorCode::NotFound => SourceError::RepositoryNotFound(repository_url.to_string()),
        _ => SourceError::GitError(format!("Clone failed: {e}")),
    })?;

    checkout_commit(&repo, commit)?;

    Ok(temp_dir)
}

fn validate_git_url(url: &Url) -> Result<(), SourceError> {
    match url.scheme() {
        "https" | "http" => {}
        scheme => {
            return Err(SourceError::GitError(format!(
                "Unsupported URL scheme: {scheme}. Only HTTP(S) is supported."
            )));
        }
    }

    if url.host_str().is_none() {
        return Err(SourceError::GitError(
            "Invalid URL: no host specified".to_string(),
        ));
    }

    Ok(())
}

fn checkout_commit(repo: &Repository, commit: &str) -> Result<(), SourceError> {
    let obj = repo.revparse_single(commit).map_err(|e| match e.code() {
        git2::ErrorCode::NotFound => SourceError::CommitNotFound(commit.to_string()),
        _ => SourceError::GitError(format!("Failed to find commit: {e}")),
    })?;

    repo.checkout_tree(&obj, None)
        .map_err(|e| SourceError::GitError(format!("Checkout failed: {e}")))?;

    repo.set_head_detached(obj.id())
        .map_err(|e| SourceError::GitError(format!("Failed to update HEAD: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_validate_git_url_valid() {
        let urls = vec![
            "https://github.com/user/repo.git",
            "https://gitlab.com/user/repo",
            "http://example.com/repo.git",
        ];

        for url_str in urls {
            let url = Url::parse(url_str).unwrap();
            assert!(validate_git_url(&url).is_ok());
        }
    }

    #[tokio::test]
    async fn test_validate_git_url_invalid_schemes() {
        let urls = vec![
            "git://github.com/user/repo.git",
            "ssh://git@github.com/user/repo.git",
            "file:///local/repo",
            "ftp://example.com/repo",
        ];

        for url_str in urls {
            let url = Url::parse(url_str).unwrap();
            assert!(matches!(
                validate_git_url(&url),
                Err(SourceError::GitError(msg)) if msg.contains("Unsupported URL scheme")
            ));
        }
    }
}

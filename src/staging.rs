use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const BUDGET_UNIT: u64 = 1024 * 1024;
/// Every file/dir the service creates in the staging dir starts with this prefix,
/// so startup cleanup never touches anything else.
const PREFIX: &str = "ms-";

/// Local scratch area for copies of source files. A byte budget (in MiB units)
/// applies back-pressure so parallel workers cannot fill the disk.
#[derive(Clone)]
pub struct Staging {
    dir: PathBuf,
    budget: Arc<Semaphore>,
    total_units: u32,
}

/// A copied file. The copy is deleted when this value is dropped.
pub struct StagedFile {
    path: PathBuf,
    _permit: OwnedSemaphorePermit,
}

/// A temporary directory inside staging (e.g. extracted video frames), deleted on drop.
pub struct StagedDir {
    path: PathBuf,
}

impl Staging {
    pub fn new(dir: &Path, max_bytes: u64) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let total_units = (max_bytes / BUDGET_UNIT).clamp(1, Semaphore::MAX_PERMITS as u64) as u32;
        Ok(Self {
            dir: dir.to_path_buf(),
            budget: Arc::new(Semaphore::new(total_units as usize)),
            total_units,
        })
    }

    /// Removes leftovers from a previous run (crash or kill).
    pub fn cleanup_leftovers(&self) -> anyhow::Result<usize> {
        let mut removed = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
                continue;
            }
            let path = entry.path();
            let res = if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match res {
                Ok(()) => removed += 1,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "staging cleanup failed")
                }
            }
        }
        Ok(removed)
    }

    /// Copies `src` into staging and returns the copy together with its SHA-256.
    /// Waits while the staging budget is exhausted.
    pub async fn copy_in(&self, src: &Path, size: u64) -> anyhow::Result<(StagedFile, String)> {
        let units = size.div_ceil(BUDGET_UNIT).clamp(1, self.total_units as u64) as u32;
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(units)
            .await
            .context("staging budget closed")?;

        let ext = src
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_ascii_lowercase()))
            .unwrap_or_default();
        let dest = self
            .dir
            .join(format!("{PREFIX}{}{ext}", uuid::Uuid::new_v4()));
        let staged = StagedFile {
            path: dest,
            _permit: permit,
        };

        let mut reader = tokio::fs::File::open(src)
            .await
            .with_context(|| format!("opening {}", src.display()))?;
        let mut writer = tokio::fs::File::create(&staged.path).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n]).await?;
        }
        writer.flush().await?;
        Ok((staged, hex::encode(hasher.finalize())))
    }

    /// SHA-256 of a file without copying it (used when `copy_on_index` is off).
    pub async fn hash_file(path: &Path) -> anyhow::Result<String> {
        let mut reader = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    pub fn temp_dir(&self) -> anyhow::Result<StagedDir> {
        let path = self
            .dir
            .join(format!("{PREFIX}dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(StagedDir { path })
    }
}

impl StagedFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %e, "failed to remove staged file");
        }
    }
}

impl StagedDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn copy_hashes_and_removes_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.TXT");
        std::fs::write(&src, b"hello").unwrap();
        let staging = Staging::new(&tmp.path().join("staging"), 10 * BUDGET_UNIT).unwrap();

        let (staged, sha) = staging.copy_in(&src, 5).await.unwrap();
        assert_eq!(
            sha,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(std::fs::read(staged.path()).unwrap(), b"hello");
        assert!(staged.path().to_string_lossy().ends_with(".txt"));
        let p = staged.path().to_path_buf();
        drop(staged);
        assert!(!p.exists());
        assert!(src.exists(), "source must never be touched");
    }

    #[test]
    fn cleanup_only_touches_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path(), BUDGET_UNIT).unwrap();
        std::fs::write(tmp.path().join("ms-leftover.jpg"), b"x").unwrap();
        std::fs::create_dir(tmp.path().join("ms-dir-1")).unwrap();
        std::fs::write(tmp.path().join("keep.txt"), b"x").unwrap();
        assert_eq!(staging.cleanup_leftovers().unwrap(), 2);
        assert!(tmp.path().join("keep.txt").exists());
    }
}

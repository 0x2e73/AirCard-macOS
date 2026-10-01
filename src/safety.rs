//! Durable recovery records and a cross-process device-operation lock.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::airlift::{BooksSnapshot, TRACKED_BOOKS_FILES};

pub fn file_key(value: &str) -> String {
    value.bytes().map(|b| format!("{b:02x}")).collect()
}

pub fn atomic_create(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("Backup path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(path)
        .map_err(|e| e.error)
        .with_context(|| {
            format!(
                "Could not create backup {}; existing backups are never overwritten",
                path.display()
            )
        })?;
    sync_directory(parent)?;
    ensure!(
        fs::read(path)? == data,
        "Backup read-back verification failed"
    );
    Ok(())
}

pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "Backup exceeds the supported size limit"
    );
    Ok(data)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn remove_durable(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    sync_directory(path.parent().context("Recovery file has no parent")?)
}

pub struct OperationGuard {
    _file: File,
}

impl OperationGuard {
    pub fn acquire(udid: &str) -> Result<Self> {
        Self::at(&crate::platform::data_dir(), udid)
    }

    fn at(root: &Path, udid: &str) -> Result<Self> {
        ensure!(
            !udid.is_empty() && udid.len() <= 128,
            "Invalid device identifier"
        );
        let dir = root.join("locks");
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("{}.lock", file_key(&udid.to_ascii_lowercase()))))?;
        file.try_lock()
            .context("Another AirCard operation is using this iPhone")?;
        Ok(Self { _file: file })
    }
}

#[derive(Serialize, Deserialize)]
pub struct PendingBooks {
    schema: u8,
    pub udid: String,
    pub snapshot: BooksSnapshot,
}

impl PendingBooks {
    pub fn path(udid: &str) -> PathBuf {
        crate::platform::data_dir()
            .join("recovery")
            .join(format!("{}.json", file_key(&udid.to_ascii_lowercase())))
    }

    pub fn ensure_clear(udid: &str) -> Result<()> {
        ensure!(
            !Self::path(udid).try_exists()?
                && !crate::protected_files::recovery_path(udid).try_exists()?,
            "An unfinished operation has recovery data. Use Recover Interrupted Operation before another change."
        );
        Ok(())
    }

    pub fn create(udid: &str, snapshot: BooksSnapshot) -> Result<Self> {
        Self::ensure_clear(udid)?;
        let pending = Self {
            schema: 1,
            udid: udid.to_string(),
            snapshot,
        };
        pending.validate(udid)?;
        atomic_create(&Self::path(udid), &serde_json::to_vec(&pending)?)?;
        Ok(pending)
    }

    pub fn load(udid: &str) -> Result<Self> {
        let pending: Self =
            serde_json::from_slice(&read_bounded(&Self::path(udid), 512 * 1024 * 1024)?)?;
        pending.validate(udid)?;
        Ok(pending)
    }

    fn validate(&self, udid: &str) -> Result<()> {
        ensure!(
            self.schema == 1 && self.udid.eq_ignore_ascii_case(udid),
            "Recovery backup belongs to a different device or format"
        );
        ensure!(
            self.snapshot.files.len() == TRACKED_BOOKS_FILES.len()
                && TRACKED_BOOKS_FILES
                    .iter()
                    .all(|p| self.snapshot.files.contains_key(*p)),
            "Recovery backup contains missing or unexpected paths"
        );
        Ok(())
    }

    pub fn complete(&self) -> Result<()> {
        ensure!(
            !crate::protected_files::recovery_path(&self.udid).try_exists()?,
            "Wallet export recovery must finish before clearing Books recovery"
        );
        remove_durable(&Self::path(&self.udid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_is_immutable_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.json");
        atomic_create(&path, b"original").unwrap();
        assert!(atomic_create(&path, b"replacement").is_err());
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[test]
    fn only_one_operation_per_device() {
        let dir = tempfile::tempdir().unwrap();
        let first = OperationGuard::at(dir.path(), "ABC").unwrap();
        assert!(OperationGuard::at(dir.path(), "abc").is_err());
        assert!(OperationGuard::at(dir.path(), "other").is_ok());
        drop(first);
        assert!(OperationGuard::at(dir.path(), "abc").is_ok());
    }

    #[test]
    fn recovery_rejects_foreign_devices_and_arbitrary_paths() {
        let files = TRACKED_BOOKS_FILES
            .iter()
            .map(|p| (p.to_string(), None))
            .collect();
        let mut record = PendingBooks {
            schema: 1,
            udid: "phone".into(),
            snapshot: BooksSnapshot { files },
        };
        assert!(record.validate("phone").is_ok());
        assert!(record.validate("other").is_err());
        record
            .snapshot
            .files
            .insert("../../unrelated".into(), Some(vec![1]));
        assert!(record.validate("phone").is_err());
    }
}

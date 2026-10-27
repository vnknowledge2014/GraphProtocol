//! Online SQLite backup (`backup_to`) and verified atomic restore (`restore_from`)
//! (`ING-064`, `INV-15`, `m2-01-design.md` §8).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;

use crate::error::StoreError;
use crate::sqlite::{
    DEFAULT_BUSY_TIMEOUT_MS, FORMAT_VERSION, SQLITE_MAGIC_HEADER, SqliteContextStore,
    map_sqlite_err, schema,
};

const SQLITE_HEADER_MIN_BYTES: usize = 100;
const BACKUP_PAGES_PER_STEP: i32 = 256;
const MAX_BACKUP_LOCK_RETRIES: u32 = 10_000;

struct StagingCleanupGuard {
    path: PathBuf,
    armed: bool,
}

impl StagingCleanupGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
            let mut wal = self.path.as_os_str().to_os_string();
            wal.push("-wal");
            let _ = fs::remove_file(PathBuf::from(wal));
            let mut shm = self.path.as_os_str().to_os_string();
            shm.push("-shm");
            let _ = fs::remove_file(PathBuf::from(shm));
        }
    }
}

fn sidecar_path(base: &Path, suffix: &str) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn path_or_wal_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
        || fs::symlink_metadata(sidecar_path(path, "-wal")).is_ok()
        || fs::symlink_metadata(sidecar_path(path, "-shm")).is_ok()
}

fn ensure_parent_dir(path: &Path) -> Result<PathBuf, StoreError> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !parent.exists() {
        fs::create_dir_all(&parent).map_err(|_| StoreError::Backend)?;
    }
    Ok(parent)
}

fn verify_sqlite_header(path: &Path) -> Result<(), StoreError> {
    let bytes = fs::read(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => StoreError::NotFound,
        _ => StoreError::Backend,
    })?;
    if bytes.len() < SQLITE_HEADER_MIN_BYTES || &bytes[..16] != SQLITE_MAGIC_HEADER {
        return Err(StoreError::Corrupt);
    }
    Ok(())
}

fn map_verify_err(err: rusqlite::Error) -> StoreError {
    match map_sqlite_err(err) {
        StoreError::Busy => StoreError::Busy,
        _ => StoreError::Corrupt,
    }
}

fn validate_staged_backup(staging_path: &Path) -> Result<(), StoreError> {
    verify_sqlite_header(staging_path)?;

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(staging_path, flags).map_err(map_verify_err)?;
    conn.busy_timeout(Duration::from_millis(u64::from(DEFAULT_BUSY_TIMEOUT_MS)))
        .map_err(map_verify_err)?;
    conn.execute_batch(
        "PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;",
    )
    .map_err(map_verify_err)?;

    let integrity_rows: Vec<String> = {
        let mut stmt = conn
            .prepare("PRAGMA integrity_check(10);")
            .map_err(map_verify_err)?;
        let mapped = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_verify_err)?;
        let mut rows = Vec::new();
        for item in mapped {
            rows.push(item.map_err(map_verify_err)?);
        }
        rows
    };

    if integrity_rows.len() != 1 || !integrity_rows[0].eq_ignore_ascii_case("ok") {
        tracing::warn!(
            findings = integrity_rows.len(),
            "backup failed sqlite integrity_check"
        );
        return Err(StoreError::Corrupt);
    }

    let version: i64 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .map_err(map_verify_err)?;
    let version_u32 = u32::try_from(version).map_err(|_| StoreError::Corrupt)?;
    if version_u32 == 0 {
        return Err(StoreError::Corrupt);
    }
    if version_u32 > FORMAT_VERSION {
        return Err(StoreError::FormatTooNew {
            found: version_u32,
            supported: FORMAT_VERSION,
        });
    }

    schema::verify_required_tables(&conn)?;
    Ok(())
}

impl SqliteContextStore {
    /// Creates a consistent online backup of the store at `dest` using SQLite's online backup API
    /// (`m2-01-design.md` §8).
    ///
    /// Safe to invoke concurrently while writers commit transactions on the store. Refuses an
    /// existing `dest` path with [`StoreError::Conflict`].
    pub fn backup_to(&self, dest: impl AsRef<Path>) -> Result<(), StoreError> {
        let dest_path = dest.as_ref();
        if path_or_wal_exists(dest_path) {
            return Err(StoreError::Conflict);
        }

        let parent = ensure_parent_dir(dest_path)?;
        let staging_path = parent.join(format!(".gap-backup-{}.tmp", Uuid::new_v4()));
        let mut guard = StagingCleanupGuard::new(staging_path.clone());

        {
            let reader_guard = self.readers.checkout();
            let src_conn = reader_guard.conn()?;

            let dst_flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let mut dst_conn =
                Connection::open_with_flags(&staging_path, dst_flags).map_err(map_sqlite_err)?;
            dst_conn
                .busy_timeout(Duration::from_millis(u64::from(
                    self.config.busy_timeout_ms,
                )))
                .map_err(map_sqlite_err)?;
            dst_conn
                .execute_batch(
                    "PRAGMA trusted_schema = OFF;
                     PRAGMA cell_size_check = ON;
                     PRAGMA journal_mode = DELETE;
                     PRAGMA synchronous = FULL;",
                )
                .map_err(map_sqlite_err)?;

            let backup = Backup::new(src_conn, &mut dst_conn).map_err(map_sqlite_err)?;
            let mut retries: u32 = 0;
            loop {
                match backup.step(BACKUP_PAGES_PER_STEP).map_err(map_sqlite_err)? {
                    StepResult::Done => break,
                    StepResult::More => {
                        retries = 0;
                    }
                    StepResult::Busy | StepResult::Locked => {
                        retries = retries.saturating_add(1);
                        if retries > MAX_BACKUP_LOCK_RETRIES {
                            return Err(StoreError::Busy);
                        }
                        thread::yield_now();
                    }
                    _ => return Err(StoreError::Backend),
                }
            }
        }

        link_staging_without_overwrite(&staging_path, dest_path, &mut guard)
    }

    /// Restores a SQLite backup from `src` to `target` (`m2-01-design.md` §8).
    ///
    /// Refuses an existing `target` with [`StoreError::Conflict`], stages a copy, runs
    /// `PRAGMA integrity_check` and the `PRAGMA user_version` check, and returns
    /// [`StoreError::Corrupt`] (or [`StoreError::FormatTooNew`]) without creating `target` if
    /// verification fails.
    pub fn restore_from(src: impl AsRef<Path>, target: impl AsRef<Path>) -> Result<(), StoreError> {
        let src_path = src.as_ref();
        let target_path = target.as_ref();

        if path_or_wal_exists(target_path) {
            return Err(StoreError::Conflict);
        }
        if !src_path.exists() {
            return Err(StoreError::NotFound);
        }

        let parent = ensure_parent_dir(target_path)?;
        let staging_path = parent.join(format!(".gap-restore-{}.tmp", Uuid::new_v4()));
        let mut guard = StagingCleanupGuard::new(staging_path.clone());

        fs::copy(src_path, &staging_path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => StoreError::NotFound,
            _ => StoreError::Backend,
        })?;

        validate_staged_backup(&staging_path)?;
        link_staging_without_overwrite(&staging_path, target_path, &mut guard)
    }
}

fn link_staging_without_overwrite(
    staging_path: &Path,
    final_path: &Path,
    guard: &mut StagingCleanupGuard,
) -> Result<(), StoreError> {
    if path_or_wal_exists(final_path) {
        return Err(StoreError::Conflict);
    }
    match fs::hard_link(staging_path, final_path) {
        Ok(()) => {
            // `staging_path` stays armed so Drop removes the staging link while `final_path`
            // retains the validated inode.
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Err(StoreError::Conflict),
        Err(err) if err.kind() == ErrorKind::Unsupported => {
            if path_or_wal_exists(final_path) {
                return Err(StoreError::Conflict);
            }
            fs::rename(staging_path, final_path).map_err(|_| StoreError::Backend)?;
            guard.disarm();
            Ok(())
        }
        Err(_) => Err(StoreError::Backend),
    }
}

//! Isolated read-only memory mapping for `.safetensors` model weights (`ADR-002`, `ARC-003`, `WP-M5-02`).
//!
//! Every `unsafe` declaration/block in this file is tracked under `UNS-001`..`UNS-005` in
//! `docs/UNSAFE.md` and wrapped inside the safe [`MmapBytes`] RAII handle.

use std::path::Path;

use gap_core::error::{ErrorCode, GapError, GapResult};

#[cfg(all(unix, target_pointer_width = "64"))]
mod imp {
    use core::ffi::{c_int, c_void};
    use core::ptr::NonNull;
    use core::slice;
    use std::fs::File;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    use gap_core::error::{ErrorCode, GapError, GapResult};

    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;

    // UNS-001 (docs/UNSAFE.md): POSIX mmap/munmap ABI from standard system libc (`libSystem` on macOS, `libc` on Linux).
    // SAFETY: Signatures match IEEE Std 1003.1 on 64-bit Unix targets where `size_t = usize` and `off_t = i64`.
    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    /// Read-only memory-mapped byte buffer backed by an open regular file.
    #[derive(Debug)]
    pub struct MmapBytes {
        ptr: NonNull<u8>,
        len: usize,
        _file: File,
    }

    // UNS-005 (docs/UNSAFE.md)
    // SAFETY: `MmapBytes` holds a `PROT_READ | MAP_PRIVATE` mapping with zero interior mutability
    // or thread-affine state. Moving ownership across threads is safe.
    unsafe impl Send for MmapBytes {}

    // UNS-005 (docs/UNSAFE.md)
    // SAFETY: Shared references `&MmapBytes` expose only immutable `&[u8]` slices to read-only pages.
    unsafe impl Sync for MmapBytes {}

    impl MmapBytes {
        /// Maps the entire regular file at `path` read-only via POSIX `mmap`.
        ///
        /// # Errors
        ///
        /// Returns [`GapError`] with [`ErrorCode::DiscoveryDegraded`] if the file cannot be opened,
        /// is not a regular non-empty file, exceeds `isize::MAX`, or `mmap` fails.
        pub fn open(path: &Path) -> GapResult<Self> {
            let file = File::open(path).map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("failed to open S1 weights `{}`: {err}", path.display()),
                )
            })?;
            let meta = file.metadata().map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("failed to stat S1 weights `{}`: {err}", path.display()),
                )
            })?;
            if !meta.is_file() {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("S1 weights path `{}` is not a regular file", path.display()),
                ));
            }
            let len_u64 = meta.len();
            if len_u64 == 0 {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("S1 weights file `{}` is empty", path.display()),
                ));
            }
            let len = usize::try_from(len_u64).map_err(|_| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!(
                        "S1 weights file `{}` exceeds addressable size",
                        path.display()
                    ),
                )
            })?;
            if len > isize::MAX as usize {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("S1 weights file `{}` exceeds slice limit", path.display()),
                ));
            }

            let fd = file.as_raw_fd();
            // UNS-002 (docs/UNSAFE.md)
            // SAFETY: `file` is a valid open regular file descriptor with verified byte length `0 < len <= isize::MAX`;
            // `addr = NULL` lets the OS select a fresh page-aligned read-only private mapping starting at offset 0.
            let mapped = unsafe { mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
            let map_failed = usize::MAX as *mut c_void;
            if mapped == map_failed || mapped.is_null() {
                let os_err = std::io::Error::last_os_error();
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("POSIX mmap failed for `{}`: {os_err}", path.display()),
                ));
            }
            let Some(ptr) = NonNull::new(mapped.cast::<u8>()) else {
                // Defensive check already guarded by `!mapped.is_null()`.
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "POSIX mmap returned null pointer",
                ));
            };

            Ok(Self {
                ptr,
                len,
                _file: file,
            })
        }

        /// Returns the mapped file contents as an immutable byte slice.
        #[must_use]
        pub fn as_slice(&self) -> &[u8] {
            // UNS-003 (docs/UNSAFE.md)
            // SAFETY: `self.ptr` was returned by a successful `PROT_READ | MAP_PRIVATE` `mmap` of length
            // `self.len <= isize::MAX`, remains mapped until `Drop`, and `&self` guarantees immutability
            // for the entire returned slice lifetime.
            unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
        }
    }

    impl Drop for MmapBytes {
        fn drop(&mut self) {
            // UNS-004 (docs/UNSAFE.md)
            // SAFETY: `self.ptr` and `self.len` come exclusively from `UNS-002`'s successful `mmap` call,
            // `Drop` runs at most once after all `&[u8]` borrows from `as_slice` have expired.
            unsafe {
                let _ = munmap(self.ptr.as_ptr().cast(), self.len);
            }
        }
    }
}

#[cfg(not(all(unix, target_pointer_width = "64")))]
mod imp {
    use std::fs;
    use std::path::Path;

    use gap_core::error::{ErrorCode, GapError, GapResult};

    /// Portable safe fallback when not running on 64-bit Unix.
    #[derive(Debug)]
    pub struct MmapBytes {
        bytes: Vec<u8>,
    }

    impl MmapBytes {
        /// Reads the entire regular file at `path` into memory.
        ///
        /// # Errors
        ///
        /// Returns [`GapError`] if reading fails or the file is empty.
        pub fn open(path: &Path) -> GapResult<Self> {
            let meta = fs::metadata(path).map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("failed to stat S1 weights `{}`: {err}", path.display()),
                )
            })?;
            if !meta.is_file() || meta.len() == 0 {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("S1 weights path `{}` is missing or empty", path.display()),
                ));
            }
            let bytes = fs::read(path).map_err(|err| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("failed to read S1 weights `{}`: {err}", path.display()),
                )
            })?;
            Ok(Self { bytes })
        }

        /// Returns the file contents as a byte slice.
        #[must_use]
        pub fn as_slice(&self) -> &[u8] {
            &self.bytes
        }
    }
}

pub use imp::MmapBytes;

impl MmapBytes {
    /// Opens `path` as a read-only mapping after confirming the descriptor exists.
    pub fn open_verified(path: &Path) -> GapResult<Self> {
        // Explicit check so missing paths report clean metadata.
        if !path.exists() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("S1 weights file `{}` does not exist", path.display()),
            ));
        }
        Self::open(path)
    }

    /// Returns the mapped byte length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// Returns `true` if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_file_path(suffix: &str) -> std::path::PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gap_s1_mmap_test_{}_{}_{suffix}",
            std::process::id(),
            id
        ))
    }

    // spec: ARC-003
    // Design ref: ADR-002
    #[test]
    fn mmap_bytes_reads_exact_payload_and_rejects_empty_file() {
        let path = temp_file_path("ok.bin");
        let payload = b"GAP-S1-MMAP-HEADER-0123456789ABCDEF";
        fs::write(&path, payload).expect("write temp file");
        let mapped = MmapBytes::open(&path).expect("mmap temp file");
        assert_eq!(mapped.len(), payload.len());
        assert_eq!(mapped.as_slice(), payload);
        drop(mapped);
        let _ = fs::remove_file(&path);

        let empty_path = temp_file_path("empty.bin");
        fs::write(&empty_path, []).expect("write empty file");
        let err = MmapBytes::open(&empty_path).expect_err("empty file must fail");
        assert_eq!(err.code(), ErrorCode::DiscoveryDegraded);
        let _ = fs::remove_file(&empty_path);
    }

    // spec: ARC-003
    // Design ref: ADR-002
    #[test]
    fn mmap_bytes_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MmapBytes>();
    }
}

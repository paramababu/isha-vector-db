//! Positional I/O and the durability primitives `std` does not expose.

use std::fs::File;
use std::io;
use std::path::Path;

/// Read at an absolute offset, without a shared cursor.
///
/// A cursor cannot be used safely from several reader threads on one handle, which is the whole
/// reason the [`File`](isha_vector_db_core::storage::File) trait is positional.
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        // `read_at` may return short for reasons other than end-of-file, so loop until the
        // buffer is full or the file really has run out. A short read treated as EOF would show
        // up as spurious "truncated file" corruption under load.
        let mut total = 0usize;
        while total < buf.len() {
            let Some(slice) = buf.get_mut(total..) else {
                break;
            };
            match file.read_at(slice, offset + total as u64) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut total = 0usize;
        while total < buf.len() {
            let Some(slice) = buf.get_mut(total..) else {
                break;
            };
            match file.seek_read(slice, offset + total as u64) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional reads are unavailable",
        ))
    }
}

/// Write every byte at an absolute offset.
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let mut written = 0usize;
        while written < buf.len() {
            let Some(slice) = buf.get(written..) else {
                break;
            };
            match file.write_at(slice, offset + written as u64) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote no bytes")),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut written = 0usize;
        while written < buf.len() {
            let Some(slice) = buf.get(written..) else {
                break;
            };
            match file.seek_write(slice, offset + written as u64) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote no bytes")),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional writes are unavailable",
        ))
    }
}

/// Make a file's data durable.
///
/// On Darwin this is `fcntl(F_FULLFSYNC)`, not `fsync`. Apple's `fsync` returns once the data
/// has reached the drive, without waiting for the drive to commit it from its own write cache —
/// so a power cut can still lose writes the caller was told were safe. `F_FULLFSYNC` is the
/// documented way to ask for the stronger guarantee, and it is the one `Durability::Full`
/// promises. It is slower, which is exactly why the engine defaults to `Durability::Batch`
/// rather than syncing on every write.
pub(crate) fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `fd` is a valid descriptor owned by `file` and outlives the call. F_FULLFSYNC
        // takes no pointer arguments, so there is nothing for the kernel to misinterpret.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC, 0) };
        if rc == 0 {
            return Ok(());
        }
        // Some filesystems — notably network mounts and some sandboxed containers — reject
        // F_FULLFSYNC with ENOTSUP or EINVAL. Falling back to fsync gives the best guarantee
        // that filesystem can offer, which beats failing the write outright.
        let err = io::Error::last_os_error();
        if !matches!(
            err.raw_os_error(),
            Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
        ) {
            return Err(err);
        }
    }
    file.sync_data()
}

/// Make a directory's own entries durable.
///
/// On POSIX a rename or a file creation is not durable until the containing directory is
/// synced; without this, a crash can leave a manifest that was renamed into place before the
/// rename itself reached the disk. A no-op on Windows, where directories cannot be opened as
/// files and the semantics differ.
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let dir = File::open(path)?;
        dir.sync_all()
    }
    #[cfg(not(unix))]
    {
        // Confirm the directory exists so the call still reports a missing path, matching the
        // trait's contract and the conformance suite.
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() {
            Ok(())
        } else {
            // `ErrorKind::NotADirectory` would say this precisely and is stable only since
            // 1.83, five releases past this crate's MSRV. `InvalidInput` is the closest kind
            // available at 1.78, and the message carries the detail that the kind cannot.
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a directory",
            ))
        }
    }
}

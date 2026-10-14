//! Mining-recall reads through an `O_DIRECT` descriptor.
//!
//! A recall range is read once per step. A buffered `pread` of that range
//! puts the pages into the cache, and nothing else reads them again. Those
//! pages evict file data that is reused, including RocksDB SST pages.
//!
//! The second descriptor is read-only and is used only by mining recall.
//! Serving, the entropy sweep, and packing validation stay on the buffered
//! descriptor.
//!
//! A buffered write that has not been `fdatasync`'d can still have dirty
//! pages for the range. Linux writes that range back before the direct read
//! (XFS and ext4 iomap), so the read returns the bytes last written, not the
//! bytes last synced. The caller overlays `pending.chunks` after the read.
//! Those bytes are newer than the file. `written_ahead` is not in that
//! overlay: the chunks were `pwrite`n and may still be dirty, and this read
//! is what returns them.
//!
//! The checksum check uses the bytes returned here. It does not read the
//! page cache.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Once;

use eyre::Context as _;

/// Logical-block alignment required for an `O_DIRECT` recall `pread`.
///
/// Recall ranges are whole chunks. A 256 KiB chunk is a multiple of this.
pub(super) const RECALL_DIRECT_ALIGN: usize = 4096;

pub(super) const MODE_DIRECT: &str = "direct";
pub(super) const MODE_BUFFERED: &str = "buffered";
pub(super) const MODE_FALLBACK: &str = "fallback";

#[cfg(test)]
thread_local! {
    static FORCE_DIRECT_OPEN_ERRNO: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// Test hook. The next `open_recall_direct` on this thread fails with `EINVAL`.
#[cfg(test)]
pub(super) fn force_recall_direct_open_einval(force: bool) {
    force_recall_direct_open_errno(force.then_some(libc::EINVAL));
}

/// Test hook. `Some(errno)` makes the next `open_recall_direct` on this thread fail.
#[cfg(test)]
pub(super) fn force_recall_direct_open_errno(errno: Option<i32>) {
    FORCE_DIRECT_OPEN_ERRNO.with(|cell| cell.set(errno));
}

/// `true` when `file` was opened with `O_DIRECT`.
///
/// Production recall does not need this. The bench checks the descriptor
/// before it stores it, and tests check the descriptor they opened.
#[cfg(any(test, feature = "bench"))]
pub(super) fn fd_is_direct(file: &File) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd as _;
        // Safety: `file` owns a live descriptor. `F_GETFL` does not change it.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        flags >= 0 && (flags & libc::O_DIRECT) != 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        false
    }
}

/// Open `path` read-only with `O_DIRECT`.
///
/// `Ok(None)` means the filesystem rejected `O_DIRECT` (`EINVAL` or
/// `EOPNOTSUPP`, for example tmpfs). The caller keeps the buffered
/// descriptor. One warning is logged for the process. Any other error is
/// returned.
pub fn open_recall_direct(path: &Path) -> eyre::Result<Option<File>> {
    match open_direct_file(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if direct_open_unsupported(&error) => {
            warn_direct_open_unsupported(path, &error);
            Ok(None)
        }
        Err(error) => {
            Err(error).wrap_err_with(|| format!("failed to open {} with O_DIRECT", path.display()))
        }
    }
}

fn direct_open_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP))
}

fn warn_direct_open_unsupported(path: &Path, error: &io::Error) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "O_DIRECT open failed; mining recall stays on the buffered descriptor"
        );
    });
}

#[cfg(target_os = "linux")]
fn open_direct_file(path: &Path) -> io::Result<File> {
    #[cfg(test)]
    if let Some(errno) = FORCE_DIRECT_OPEN_ERRNO.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_direct_file(path: &Path) -> io::Result<File> {
    let _ = path;
    #[cfg(test)]
    if let Some(errno) = FORCE_DIRECT_OPEN_ERRNO.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    Err(io::Error::from_raw_os_error(libc::EINVAL))
}

/// Bytes from one recall `pread`, and the mode that produced them.
pub(super) struct RecallBytes {
    storage: Vec<u8>,
    start: usize,
    end: usize,
    pub mode: &'static str,
}

impl RecallBytes {
    fn exact(bytes: Vec<u8>, mode: &'static str) -> Self {
        let end = bytes.len();
        Self {
            storage: bytes,
            start: 0,
            end,
            mode,
        }
    }

    fn aligned(storage: Vec<u8>, start: usize, end: usize) -> Self {
        Self {
            storage,
            start,
            end,
            mode: MODE_DIRECT,
        }
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.storage[self.start..self.end]
    }
}

/// One mining-recall `pread`.
///
/// See the module comment for the dirty-page rule. An unaligned offset or
/// length, or `EINVAL` from the direct read, uses `buffered` and is `fallback`.
/// `direct == None` with `direct_requested` is the open-failure fallback.
pub(super) fn read_recall_piece(
    buffered: &File,
    direct: Option<&File>,
    direct_requested: bool,
    offset: u64,
    nbytes: usize,
) -> io::Result<RecallBytes> {
    if !direct_requested {
        return read_buffered(buffered, offset, nbytes)
            .map(|bytes| RecallBytes::exact(bytes, MODE_BUFFERED));
    }
    let Some(direct) = direct else {
        return read_buffered(buffered, offset, nbytes)
            .map(|bytes| RecallBytes::exact(bytes, MODE_FALLBACK));
    };
    if !range_is_direct_aligned(offset, nbytes) {
        return read_buffered(buffered, offset, nbytes)
            .map(|bytes| RecallBytes::exact(bytes, MODE_FALLBACK));
    }
    match read_direct(direct, offset, nbytes) {
        Ok(bytes) => Ok(bytes),
        Err(DirectReadError::Rejected(error)) => {
            warn_direct_read_rejected(offset, &error);
            read_buffered(buffered, offset, nbytes)
                .map(|bytes| RecallBytes::exact(bytes, MODE_FALLBACK))
        }
        Err(DirectReadError::Io(error)) => Err(error),
    }
}

pub(super) fn range_is_direct_aligned(offset: u64, len: usize) -> bool {
    let Ok(len) = u64::try_from(len) else {
        return false;
    };
    let align = RECALL_DIRECT_ALIGN as u64;
    len > 0 && len.is_multiple_of(align) && offset.is_multiple_of(align)
}

fn read_buffered(file: &File, offset: u64, nbytes: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0_u8; nbytes];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

enum DirectReadError {
    Rejected(io::Error),
    Io(io::Error),
}

fn read_direct(file: &File, offset: u64, nbytes: usize) -> Result<RecallBytes, DirectReadError> {
    let mut storage = vec![0_u8; nbytes.saturating_add(RECALL_DIRECT_ALIGN)];
    let align = storage.as_mut_ptr().align_offset(RECALL_DIRECT_ALIGN);
    if align == usize::MAX || align.saturating_add(nbytes) > storage.len() {
        return Err(DirectReadError::Rejected(io::Error::new(
            ErrorKind::InvalidInput,
            "could not align the O_DIRECT recall buffer to 4096 bytes",
        )));
    }
    let end = align + nbytes;
    read_exact_aligned(file, &mut storage[align..end], offset)?;
    Ok(RecallBytes::aligned(storage, align, end))
}

/// `pread` loop that keeps the buffer pointer and the offset 4096-aligned.
///
/// `read_exact_at` advances by whatever the kernel returned. A short count
/// that is not a multiple of the alignment makes the next `O_DIRECT` call
/// invalid. That short count is a rejection, and the caller reads the whole
/// range from the buffered descriptor.
fn read_exact_aligned(file: &File, buf: &mut [u8], mut offset: u64) -> Result<(), DirectReadError> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset) {
            Ok(0) => {
                return Err(DirectReadError::Io(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "O_DIRECT recall read returned 0",
                )));
            }
            Ok(n) => {
                let next = done + n;
                if next != buf.len() && !n.is_multiple_of(RECALL_DIRECT_ALIGN) {
                    return Err(DirectReadError::Rejected(io::Error::new(
                        ErrorKind::InvalidInput,
                        "O_DIRECT recall read returned a short count that is not 4096-aligned",
                    )));
                }
                done = next;
                offset += u64::try_from(n).unwrap_or(u64::MAX);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                return Err(DirectReadError::Rejected(error));
            }
            Err(error) => return Err(DirectReadError::Io(error)),
        }
    }
    Ok(())
}

fn warn_direct_read_rejected(offset: u64, error: &io::Error) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            offset,
            error = %error,
            "O_DIRECT recall read was rejected; this read used the buffered descriptor"
        );
    });
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::{force_recall_direct_open_errno, open_recall_direct, range_is_direct_aligned};
    use irys_testing_utils::utils::TempDirBuilder;

    #[test]
    fn alignment_rejects_a_short_or_shifted_range() {
        assert!(range_is_direct_aligned(0, 4096));
        assert!(range_is_direct_aligned(262_144, 262_144));
        assert!(!range_is_direct_aligned(0, 32));
        assert!(!range_is_direct_aligned(512, 4096));
        assert!(!range_is_direct_aligned(0, 0));
    }

    #[test]
    fn eopnotsupp_falls_back_like_einval() {
        let dir = TempDirBuilder::new().prefix("recall_eopnotsupp").build();
        let path = dir.path().join("chunks.dat");
        std::fs::File::create(&path)
            .expect("chunks.dat")
            .write_all(&[0])
            .expect("seed");

        force_recall_direct_open_errno(Some(libc::EOPNOTSUPP));
        let opened = open_recall_direct(&path).expect("EOPNOTSUPP falls back");
        assert!(
            opened.is_none(),
            "EOPNOTSUPP must keep the buffered descriptor"
        );
        force_recall_direct_open_errno(None);

        force_recall_direct_open_errno(Some(libc::EPERM));
        let err = open_recall_direct(&path).expect_err("EPERM stays fatal");
        let message = format!("{err:#}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        force_recall_direct_open_errno(None);
    }
}

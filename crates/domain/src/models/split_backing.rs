//! Bench-only second file for one submodule.
//!
//! Recall reads stay on a read-only entropy file at the partition offset.
//! Packed writes, the entropy pread that feeds a packed write, and Data reads
//! use the write file. Partition offset `write_origin` is byte 0 of that file.
//! One disk lane still owns both descriptors.

use std::fs::File;
use std::os::unix::io::AsRawFd as _;
use std::sync::{Arc, Mutex};

use eyre::{ensure, eyre};

/// Two files that stand in for one `chunks.dat` during a storage-module bench.
#[derive(Debug)]
pub struct SplitBacking {
    pub(crate) recall: Arc<Mutex<File>>,
    pub(crate) write: Arc<Mutex<File>>,
    /// `O_DIRECT` descriptor for the same bytes as `recall`. Mining recall
    /// uses it. `None` when the bench did not request it, or when the open
    /// failed.
    pub(super) recall_direct: Option<Arc<File>>,
    pub(crate) write_origin: u32,
    pub(crate) write_chunks: u64,
    /// File chunk of partition chunk 0. Recall reads add this to the
    /// partition-relative chunk. Packed writes do not.
    pub(crate) recall_origin: u32,
}

impl SplitBacking {
    /// `recall` must already be `O_RDONLY`. Both files are advised `RANDOM`
    /// by the storage module after this returns.
    pub fn open(
        recall: File,
        write: File,
        write_origin: u32,
        write_chunks: u64,
    ) -> eyre::Result<Self> {
        ensure_recall_rdonly(&recall)?;
        ensure!(write_chunks > 0, "write region has no chunks");
        Ok(Self {
            recall: Arc::new(Mutex::new(recall)),
            write: Arc::new(Mutex::new(write)),
            recall_direct: None,
            write_origin,
            write_chunks,
            recall_origin: 0,
        })
    }

    /// Partition chunk 0 reads file chunk `chunks` of the recall file.
    pub fn with_recall_origin(mut self, chunks: u32) -> Self {
        self.recall_origin = chunks;
        self
    }

    /// Mining-recall descriptor for the same file as `recall`.
    ///
    /// `file` must be read-only and open `O_DIRECT`. Entropy-for-XOR and serve
    /// stay on the buffered recall descriptor.
    pub fn with_direct_recall(mut self, file: File) -> eyre::Result<Self> {
        ensure_recall_rdonly(&file)?;
        ensure!(
            super::recall_direct::fd_is_direct(&file),
            "recall direct file is not open O_DIRECT"
        );
        self.recall_direct = Some(Arc::new(file));
        Ok(self)
    }

    /// Byte offset in the write file for a submodule-relative chunk.
    /// The last valid chunk ends on the last byte of the file and does not extend it.
    pub(crate) fn mapped_offset(
        &self,
        submodule_offset: u32,
        chunk_size: u64,
    ) -> eyre::Result<u64> {
        map_write_offset(
            self.write_origin,
            self.write_chunks,
            submodule_offset,
            chunk_size,
        )
    }
}

pub(crate) fn map_write_offset(
    write_origin: u32,
    write_chunks: u64,
    submodule_offset: u32,
    chunk_size: u64,
) -> eyre::Result<u64> {
    if submodule_offset < write_origin {
        return Err(eyre!(
            "partition offset {submodule_offset} is below write origin {write_origin}"
        ));
    }
    let rel = u64::from(submodule_offset - write_origin);
    if rel >= write_chunks {
        return Err(eyre!(
            "partition offset {submodule_offset} is outside the write region ({rel} >= {write_chunks})"
        ));
    }
    Ok(rel.saturating_mul(chunk_size))
}

fn ensure_recall_rdonly(file: &File) -> eyre::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(eyre!(
            "fcntl(F_GETFL) on the recall file failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    ensure!(
        flags & libc::O_ACCMODE == libc::O_RDONLY,
        "recall file is not open O_RDONLY"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn recall_must_be_read_only() {
        let read_only = File::open("/dev/null").unwrap();
        let split = SplitBacking::open(read_only, File::open("/dev/null").unwrap(), 0, 4).unwrap();
        assert_eq!(split.mapped_offset(0, 262_144).unwrap(), 0);
        assert_eq!(split.mapped_offset(3, 262_144).unwrap(), 3 * 262_144);
        assert!(split.mapped_offset(4, 262_144).is_err());
        assert!(map_write_offset(10, 4, 9, 262_144).is_err());

        let writable = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let rejected = SplitBacking::open(writable, File::open("/dev/null").unwrap(), 0, 1);
        assert!(rejected.is_err());

        let split = SplitBacking::open(
            File::open("/dev/null").unwrap(),
            File::open("/dev/null").unwrap(),
            0,
            1,
        )
        .unwrap();
        let rejected = split.with_direct_recall(File::open("/dev/null").unwrap());
        assert!(rejected.is_err());
    }
}

//! `mid_write_intervals.json`: spans whose chunk bytes may be mid-overwrite.
//!
//! An `Entropy` overwrite (data sync filling an entropy hole) appends its
//! span here, fsynced before `pwrite`, instead of rewriting the whole
//! `intervals.json` to publish an `Interrupted` mark. Open applies the list
//! over `intervals.json`: listed offsets reload `Uninitialized`, everything
//! else keeps its stored label. A completion rewrite of `intervals.json`
//! (persist timer, `force_sync`, shutdown) folds the list in and truncates it.
//!
//! One record per line: `<crc32c as 8 hex digits> <json>\n`. The JSON is a
//! list of spans in the `intervals.json` encoding, each labelled
//! `Interrupted`. Replay only relabels offsets `Uninitialized`, so it is
//! idempotent, and a line that fails its checksum is skipped rather than
//! ending the log: the records after it still guard their writes. A torn
//! last line (no newline) is ignored and cut off at open. An append that
//! finds a torn last line starts on a fresh line, so a failed append
//! cannot merge with the retry.
use super::*;

pub(super) const MID_WRITE_INTERVALS_FILE: &str = "mid_write_intervals.json";

/// In-memory copy of one submodule's log.
#[derive(Debug, Default)]
pub(super) struct MidWriteLog {
    /// Each recorded span with the sequence number that last put it in
    /// flight. Spans loaded at open carry sequence 0.
    entries: Vec<(u64, Interval<PartitionChunkOffset>)>,
    /// Union of the entries the file is known to hold. A write inside it
    /// skips the append.
    recorded: IntervalMask,
    /// A truncate at this sequence failed after it may have changed the
    /// file. Entries at or below it stay for the next truncate, but no
    /// longer count as recorded.
    untrusted_through: u64,
}

impl MidWriteLog {
    /// Spans the file now holds, appended under `seq`.
    pub(super) fn record(&mut self, seq: u64, spans: &[Interval<PartitionChunkOffset>]) {
        for span in spans {
            self.entries.push((seq, *span));
            self.recorded.insert_merge_touching_or_overlapping(*span);
        }
    }

    /// Spans already recorded and now in flight again. They take `seq`, so
    /// a replace that read an older sequence does not truncate them.
    pub(super) fn rearm(&mut self, seq: u64, spans: &[Interval<PartitionChunkOffset>]) {
        for span in spans {
            self.entries.push((seq, *span));
        }
    }

    pub(super) fn covers(&self, span: Interval<PartitionChunkOffset>) -> bool {
        self.recorded.contains_interval(span)
    }

    /// `true` when any entry lists `offset`, trusted or not.
    pub(super) fn contains_point(&self, offset: PartitionChunkOffset) -> bool {
        self.recorded.contains_point(offset)
            || (self.untrusted_through > 0
                && self
                    .entries
                    .iter()
                    .any(|(_, span)| span.contains_point(offset)))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `true` when an entry at or below `seq` is still held.
    pub(super) fn has_entries_through(&self, seq: u64) -> bool {
        self.entries.iter().any(|(entry_seq, _)| *entry_seq <= seq)
    }

    /// Spans put in flight after `seq`. A replace that read `seq` before its
    /// map copy does not cover them.
    pub(super) fn spans_after(&self, seq: u64) -> Vec<Interval<PartitionChunkOffset>> {
        self.entries
            .iter()
            .filter(|(entry_seq, _)| *entry_seq > seq)
            .map(|(_, span)| *span)
            .collect()
    }

    /// The file now holds exactly the entries after `seq`.
    pub(super) fn retain_after(&mut self, seq: u64) {
        self.entries.retain(|(entry_seq, _)| *entry_seq > seq);
        self.untrusted_through = 0;
        self.rebuild_recorded();
    }

    /// A truncate at `seq` failed. The file may already lack the entries at
    /// or below it, so they stop counting as recorded.
    pub(super) fn distrust_through(&mut self, seq: u64) {
        self.untrusted_through = self.untrusted_through.max(seq);
        self.rebuild_recorded();
    }

    fn rebuild_recorded(&mut self) {
        let mut recorded = IntervalMask::new();
        for (entry_seq, span) in &self.entries {
            if *entry_seq > self.untrusted_through {
                recorded.insert_merge_touching_or_overlapping(*span);
            }
        }
        self.recorded = recorded;
    }

    #[cfg(test)]
    pub(super) fn recorded_spans(&self) -> Vec<Interval<PartitionChunkOffset>> {
        self.recorded.iter().copied().collect()
    }
}

pub(super) fn mid_write_path(submodule_dir: &Path) -> PathBuf {
    submodule_dir.join(MID_WRITE_INTERVALS_FILE)
}

fn mid_write_error(path: &Path, action: &str) -> String {
    format!(
        "Failed to {action} {MID_WRITE_INTERVALS_FILE} at {}",
        path.display()
    )
}

/// One record line for `spans`.
fn encode_line(spans: &[Interval<PartitionChunkOffset>]) -> Result<Vec<u8>> {
    let mut merged = IntervalMask::new();
    for span in spans {
        merged.insert_merge_touching_or_overlapping(*span);
    }
    let mut labelled = StorageIntervals::new();
    for span in merged.iter() {
        labelled
            .insert_strict(*span, ChunkType::Interrupted)
            .map_err(|error| eyre!("merged mid-write spans overlap: {error:?}"))?;
    }
    let json = serde_json::to_string(&labelled)?;
    let crc = crc32c::crc32c(json.as_bytes());
    Ok(format!("{crc:08x} {json}\n").into_bytes())
}

/// Spans of one complete, checksummed line. `None` for a corrupt line.
fn decode_line(line: &[u8]) -> Option<Vec<Interval<PartitionChunkOffset>>> {
    let (crc_hex, json) = line.split_at_checked(8)?;
    let json = json.strip_prefix(b" ")?;
    let crc = u32::from_str_radix(std::str::from_utf8(crc_hex).ok()?, 16).ok()?;
    if crc32c::crc32c(json) != crc {
        return None;
    }
    let labelled: StorageIntervals = serde_json::from_slice(json).ok()?;
    Some(labelled.iter().map(|(span, _)| *span).collect())
}

/// Every span in the log at `path`. A missing file is an empty log. A
/// corrupt line is skipped. A torn last line is ignored and cut off.
pub(super) fn read_mid_write_file(path: &Path) -> Result<Vec<Interval<PartitionChunkOffset>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).wrap_err_with(|| mid_write_error(path, "read")),
    };
    let mut spans = Vec::new();
    let mut complete_len = 0_usize;
    let mut skipped = 0_usize;
    while let Some(newline) = bytes[complete_len..].iter().position(|byte| *byte == b'\n') {
        let line = &bytes[complete_len..complete_len + newline];
        complete_len += newline + 1;
        // An empty line is the separator an append adds after a torn line.
        if line.is_empty() {
            continue;
        }
        match decode_line(line) {
            Some(line_spans) => spans.extend(line_spans),
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        warn!(
            path = %path.display(),
            skipped,
            "skipping corrupt {MID_WRITE_INTERVALS_FILE} records"
        );
    }
    if complete_len < bytes.len() {
        warn!(
            path = %path.display(),
            complete_len,
            file_len = bytes.len(),
            "ignoring a torn {MID_WRITE_INTERVALS_FILE} tail"
        );
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .wrap_err_with(|| mid_write_error(path, "open"))?;
        file.set_len(u64::try_from(complete_len)?)
            .wrap_err_with(|| mid_write_error(path, "truncate the torn tail of"))?;
        file.sync_all()
            .wrap_err_with(|| mid_write_error(path, "fsync"))?;
    }
    Ok(spans)
}

/// Append one record for `spans` and fdatasync it.
///
/// A file that does not end in a newline holds a torn earlier append; the
/// record starts on a fresh line so the reader can still decode it. A
/// failed write or fdatasync cuts the file back to its earlier length. An
/// append to an empty file also fsyncs the directory, so a new file
/// survives a crash even when an earlier attempt's directory fsync failed.
/// Returns `true` when the directory was fsynced.
pub(super) fn append_mid_write(
    path: &Path,
    spans: &[Interval<PartitionChunkOffset>],
) -> Result<bool> {
    let line = encode_line(spans)?;
    let mut file = OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(path)
        .wrap_err_with(|| mid_write_error(path, "open"))?;
    let previous_len = file
        .metadata()
        .wrap_err_with(|| mid_write_error(path, "stat"))?
        .len();
    let mut record = Vec::with_capacity(line.len() + 1);
    if previous_len > 0 {
        let mut last = [0_u8; 1];
        file.read_exact_at(&mut last, previous_len - 1)
            .wrap_err_with(|| mid_write_error(path, "read the tail of"))?;
        if last[0] != b'\n' {
            record.push(b'\n');
        }
    }
    record.extend_from_slice(&line);
    let written = file
        .write_all(&record)
        .wrap_err_with(|| mid_write_error(path, "append to"))
        .and_then(|()| {
            file.sync_data()
                .wrap_err_with(|| mid_write_error(path, "fsync"))
        });
    if let Err(err) = written {
        // Best effort: a partial record left here is skipped on read, and
        // the next append starts after it on a fresh line.
        let _ = file.set_len(previous_len).and_then(|()| file.sync_data());
        return Err(err);
    }
    let dir_synced = previous_len == 0;
    if dir_synced {
        sync_parent_dir(path)?;
    }
    Ok(dir_synced)
}

/// Replace the log with `keep`. An empty `keep` truncates the file. A
/// non-empty one goes through the same atomic replace as `intervals.json`.
pub(super) fn rewrite_mid_write(
    path: &Path,
    keep: &[Interval<PartitionChunkOffset>],
) -> Result<()> {
    if keep.is_empty() {
        let file = match OpenOptions::new().write(true).open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).wrap_err_with(|| mid_write_error(path, "open")),
        };
        file.set_len(0)
            .wrap_err_with(|| mid_write_error(path, "truncate"))?;
        file.sync_all()
            .wrap_err_with(|| mid_write_error(path, "fsync"))?;
        return Ok(());
    }
    // The temp file is `.mid_write_intervals.json.*`, apart from the
    // `intervals.json` replace's temp names.
    let mut file = get_atomic_file(path)?;
    file.write_all(&encode_line(keep)?)
        .wrap_err_with(|| mid_write_error(path, "write"))?;
    file.commit()
        .wrap_err_with(|| mid_write_error(path, "replace"))
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| eyre!("{} has no parent directory", path.display()))?;
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .wrap_err_with(|| format!("Failed to fsync directory {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::utils::TempDirBuilder;

    fn span(start: u32, end: u32) -> Interval<PartitionChunkOffset> {
        ii(PartitionChunkOffset(start), PartitionChunkOffset(end))
    }

    #[test]
    fn records_round_trip_and_merge() -> Result<()> {
        let tmp = TempDirBuilder::new().prefix("mid_write_round_trip").build();
        let path = mid_write_path(tmp.path());
        assert!(read_mid_write_file(&path)?.is_empty(), "missing is empty");
        assert!(
            append_mid_write(&path, &[span(4, 4), span(5, 6)])?,
            "new file"
        );
        assert!(!append_mid_write(&path, &[span(9, 9)])?, "existing file");
        assert_eq!(read_mid_write_file(&path)?, vec![span(4, 6), span(9, 9)]);
        let text = fs::read_to_string(&path)?;
        let first = text.lines().next().expect("one line");
        let json = first.split_once(' ').expect("crc then json").1;
        let labelled: StorageIntervals = serde_json::from_str(json)?;
        assert_eq!(
            labelled.get_at_point(PartitionChunkOffset(5)).copied(),
            Some(ChunkType::Interrupted),
            "the record uses the intervals.json span and chunk-type encoding"
        );
        Ok(())
    }

    /// A corrupt record in the middle does not end the log. The file keeps
    /// it, and the later record still applies.
    #[test]
    fn corrupt_line_is_skipped_and_later_records_apply() -> Result<()> {
        let tmp = TempDirBuilder::new().prefix("mid_write_corrupt").build();
        let path = mid_write_path(tmp.path());
        append_mid_write(&path, &[span(1, 1)])?;
        let mut bad = encode_line(&[span(2, 2)])?;
        bad[0] = if bad[0] == b'0' { b'1' } else { b'0' };
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(&bad)?;
        drop(file);
        append_mid_write(&path, &[span(3, 3)])?;
        let len = fs::metadata(&path)?.len();
        assert_eq!(read_mid_write_file(&path)?, vec![span(1, 1), span(3, 3)]);
        assert_eq!(fs::metadata(&path)?.len(), len, "a middle line was cut");
        Ok(())
    }

    #[test]
    fn torn_tail_is_ignored_and_cut() -> Result<()> {
        let tmp = TempDirBuilder::new().prefix("mid_write_torn_unit").build();
        let path = mid_write_path(tmp.path());
        append_mid_write(&path, &[span(1, 1)])?;
        let good_len = fs::metadata(&path)?.len();
        let torn = encode_line(&[span(2, 2)])?;
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(&torn[..torn.len() / 2])?;
        drop(file);
        assert_eq!(read_mid_write_file(&path)?, vec![span(1, 1)]);
        assert_eq!(fs::metadata(&path)?.len(), good_len, "the torn tail stayed");
        Ok(())
    }

    /// A failed append can leave half a record. The retry must not land on
    /// the same line: the reader would drop both.
    #[test]
    fn append_after_a_torn_append_starts_a_fresh_line() -> Result<()> {
        let tmp = TempDirBuilder::new().prefix("mid_write_retry").build();
        let path = mid_write_path(tmp.path());
        append_mid_write(&path, &[span(1, 1)])?;
        let torn = encode_line(&[span(2, 2)])?;
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(&torn[..torn.len() / 2])?;
        drop(file);
        append_mid_write(&path, &[span(2, 2)])?;
        assert_eq!(read_mid_write_file(&path)?, vec![span(1, 1), span(2, 2)]);
        Ok(())
    }

    /// The directory fsync follows the file length, not whether the file
    /// exists: a first attempt can create the file and fail that fsync.
    #[test]
    fn append_to_an_empty_existing_file_fsyncs_the_directory() -> Result<()> {
        let tmp = TempDirBuilder::new().prefix("mid_write_dir_sync").build();
        let path = mid_write_path(tmp.path());
        fs::write(&path, b"")?;
        assert!(append_mid_write(&path, &[span(1, 1)])?);
        assert!(!append_mid_write(&path, &[span(2, 2)])?);
        rewrite_mid_write(&path, &[])?;
        assert!(append_mid_write(&path, &[span(3, 3)])?);
        Ok(())
    }

    #[test]
    fn rewrite_keeps_only_the_given_spans() -> Result<()> {
        let tmp = TempDirBuilder::new().prefix("mid_write_rewrite").build();
        let path = mid_write_path(tmp.path());
        append_mid_write(&path, &[span(1, 1)])?;
        rewrite_mid_write(&path, &[span(7, 8)])?;
        assert_eq!(read_mid_write_file(&path)?, vec![span(7, 8)]);
        let names: Vec<_> = fs::read_dir(tmp.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<_>>()?;
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(MID_WRITE_INTERVALS_FILE)]
        );
        rewrite_mid_write(&path, &[])?;
        assert_eq!(fs::metadata(&path)?.len(), 0, "an empty keep truncates");
        Ok(())
    }

    /// After a failed truncate the old entries stop covering writes but still
    /// mark offsets as not persisted. A later truncate trusts what it wrote.
    #[test]
    fn distrusted_entries_do_not_cover_writes() {
        let mut log = MidWriteLog::default();
        log.record(1, &[span(1, 1)]);
        assert!(log.covers(span(1, 1)));
        log.distrust_through(1);
        assert!(
            !log.covers(span(1, 1)),
            "a distrusted entry covered a write"
        );
        assert!(log.contains_point(PartitionChunkOffset(1)));
        log.record(2, &[span(5, 5)]);
        assert!(log.covers(span(5, 5)), "a later append is trusted");
        log.retain_after(0);
        assert!(log.covers(span(1, 1)), "a rewrite stores every kept entry");
        log.retain_after(1);
        assert!(!log.contains_point(PartitionChunkOffset(1)));
        assert!(log.covers(span(5, 5)));
    }
}

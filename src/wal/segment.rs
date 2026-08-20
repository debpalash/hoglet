//! WAL segment format and recovery.
//!
//! A segment is a sequence of records: `[len: u32 LE][crc32: u32 LE][payload]`.
//! `crc32` covers the payload only. Recovery scans records in order and
//! truncates the file at the first record whose length or checksum is
//! invalid — a torn or corrupted tail never becomes valid data, and nothing
//! before it is lost (claims.md claim 2).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A record longer than this is rejected at write time and treated as
/// corruption at read time. Must exceed the largest request body we accept
/// after decompression.
pub const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Segments roll when they exceed this size.
pub const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

const RECORD_HEADER_BYTES: usize = 8;

pub fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:016}.wal"))
}

/// Parse a segment file name back to its sequence number.
pub fn parse_seq(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let seq = name.strip_suffix(".wal")?;
    (seq.len() == 16).then(|| seq.parse().ok())?
}

/// Encode one record into `buf`.
pub fn encode_record(payload: &[u8], buf: &mut Vec<u8>) {
    assert!(
        payload.len() <= MAX_RECORD_BYTES,
        "record exceeds MAX_RECORD_BYTES; caller must reject earlier"
    );
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    buf.extend_from_slice(payload);
}

/// Result of scanning one segment.
pub struct ScannedSegment {
    /// Payloads of every valid record, in order.
    pub records: Vec<Vec<u8>>,
    /// Byte offset of the first invalid record, if the segment was truncated.
    pub valid_len: u64,
    pub truncated: bool,
}

/// Scan a segment, returning valid records and truncating the file itself at
/// the first invalid one so a torn tail can never be re-read as data.
pub fn scan_and_truncate(path: &Path) -> std::io::Result<ScannedSegment> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    let scanned = scan_bytes(bytes);

    if scanned.truncated {
        // Invalid tail: truncate it away, durably.
        file.set_len(scanned.valid_len)?;
        file.sync_all()?;
    }
    Ok(scanned)
}

/// Scan a legacy segment without changing it. Offline migration must be a
/// read-only observer of legacy storage, including torn tails.
pub fn scan_read_only(path: &Path) -> std::io::Result<ScannedSegment> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(scan_bytes(bytes))
}

fn scan_bytes(bytes: Vec<u8>) -> ScannedSegment {
    let mut records = Vec::new();
    let mut offset = 0usize;

    loop {
        let remaining = bytes.len() - offset;
        if remaining == 0 {
            return ScannedSegment {
                records,
                valid_len: offset as u64,
                truncated: false,
            };
        }
        if remaining < RECORD_HEADER_BYTES {
            break;
        }
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let payload_start = offset + RECORD_HEADER_BYTES;
        if len > MAX_RECORD_BYTES || payload_start + len > bytes.len() {
            break;
        }
        let payload = &bytes[payload_start..payload_start + len];
        if crc32fast::hash(payload) != crc {
            break;
        }
        records.push(payload.to_vec());
        offset = payload_start + len;
    }

    ScannedSegment {
        records,
        valid_len: offset as u64,
        truncated: true,
    }
}

/// List segment files in `dir`, ordered by sequence number.
pub fn list_segments(dir: &Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let mut segments = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if let Some(seq) = parse_seq(&path) {
            segments.push((seq, path));
        }
    }
    segments.sort_by_key(|(seq, _)| *seq);
    Ok(segments)
}

/// Open a segment for appending, positioned at its end.
pub fn open_for_append(path: &Path) -> std::io::Result<(File, u64)> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let len = file.seek(SeekFrom::End(0))?;
    Ok((file, len))
}

/// Durably create `dir` (fsyncs the parent so the directory entry survives
/// crash).
pub fn create_dir_durable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if let Some(parent) = dir.parent() {
        File::open(parent)?.sync_all()?;
    }
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Write bytes then fsync — the durability point. Ack nothing before this
/// returns.
pub fn append_durable(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_records(path: &Path, payloads: &[&[u8]]) {
        let mut buf = Vec::new();
        for p in payloads {
            encode_record(p, &mut buf);
        }
        std::fs::write(path, &buf).unwrap();
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000000001.wal");
        write_records(&path, &[b"one", b"two"]);
        let scanned = scan_and_truncate(&path).unwrap();
        assert_eq!(scanned.records, vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(!scanned.truncated);
    }

    #[test]
    fn torn_tail_truncates_to_valid_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000000001.wal");
        write_records(&path, &[b"one", b"two"]);
        // Tear the last record: chop 2 bytes off the file.
        let full = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full - 2).unwrap();

        let scanned = scan_and_truncate(&path).unwrap();
        assert_eq!(scanned.records, vec![b"one".to_vec()]);
        assert!(scanned.truncated);
        // File is physically truncated: a re-scan sees a clean segment.
        let rescan = scan_and_truncate(&path).unwrap();
        assert_eq!(rescan.records, vec![b"one".to_vec()]);
        assert!(!rescan.truncated);
    }

    #[test]
    fn corrupted_byte_truncates_at_that_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000000001.wal");
        write_records(&path, &[b"aaaa", b"bbbb", b"cccc"]);
        // Flip a byte inside record 2's payload.
        let mut bytes = std::fs::read(&path).unwrap();
        let record_size = RECORD_HEADER_BYTES + 4;
        bytes[record_size + RECORD_HEADER_BYTES] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let scanned = scan_and_truncate(&path).unwrap();
        // Record 1 survives; 2 and 3 are gone — a corrupt middle never
        // yields data after it.
        assert_eq!(scanned.records, vec![b"aaaa".to_vec()]);
        assert!(scanned.truncated);
    }

    #[test]
    fn garbage_length_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000000001.wal");
        let mut buf = Vec::new();
        encode_record(b"good", &mut buf);
        buf.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd length
        buf.extend_from_slice(&[0u8; 12]);
        std::fs::write(&path, &buf).unwrap();
        let scanned = scan_and_truncate(&path).unwrap();
        assert_eq!(scanned.records, vec![b"good".to_vec()]);
        assert!(scanned.truncated);
    }

    #[test]
    fn empty_segment_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000000001.wal");
        std::fs::write(&path, b"").unwrap();
        let scanned = scan_and_truncate(&path).unwrap();
        assert!(scanned.records.is_empty());
        assert!(!scanned.truncated);
    }

    #[test]
    fn segment_listing_sorts_by_seq() {
        let dir = tempfile::tempdir().unwrap();
        for seq in [3u64, 1, 2] {
            std::fs::write(segment_path(dir.path(), seq), b"").unwrap();
        }
        std::fs::write(dir.path().join("not-a-segment.txt"), b"").unwrap();
        let segments = list_segments(dir.path()).unwrap();
        let seqs: Vec<u64> = segments.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}

//! Byte-accounted write-ahead log used by the durable event pipeline.
//!
//! A cursor always names the next unread byte. Records are self-describing:
//! `[magic:4][version:1][reserved:3][length:u32 LE][crc32:u32 LE][JSON]`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::event::CapturedEvent;

const RECORD_MAGIC: [u8; 4] = *b"HGLW";
const RECORD_VERSION: u8 = 1;
const RECORD_HEADER_BYTES: usize = 16;
const OPEN_SUFFIX: &str = ".open";
const SEALED_SUFFIX: &str = ".wal";
const WRITER_LOCK_FILE: &str = ".writer.lock";

/// Events accepted together at the capture boundary and persisted as one WAL
/// record. Publication never splits a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedBatch {
    pub events: Vec<CapturedEvent>,
    /// Durable token-to-project bindings established by capture authorization.
    /// Legacy WAL records deserialize without bindings and must be handled by
    /// offline migration rather than the v2 publication coordinator.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    project_ids_by_token: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    historical_migration: bool,
}

impl CapturedBatch {
    pub fn new(events: Vec<CapturedEvent>) -> Result<Self, WalError> {
        if events.is_empty() {
            return Err(WalError::EmptyBatch);
        }
        Ok(Self {
            events,
            project_ids_by_token: BTreeMap::new(),
            historical_migration: false,
        })
    }

    /// Construct a batch whose project authority remains provable after a
    /// project token is rotated or removed from Control State.
    pub fn authorized(
        events: Vec<CapturedEvent>,
        project_ids_by_token: BTreeMap<String, String>,
        historical_migration: bool,
    ) -> Result<Self, WalError> {
        if events.is_empty() {
            return Err(WalError::EmptyBatch);
        }
        if project_ids_by_token
            .iter()
            .any(|(token, project_id)| token.is_empty() || project_id.trim().is_empty())
            || events
                .iter()
                .any(|event| !project_ids_by_token.contains_key(&event.token))
        {
            return Err(WalError::InvalidProjectBinding);
        }
        Ok(Self {
            events,
            project_ids_by_token,
            historical_migration,
        })
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn project_id_for(&self, event: &CapturedEvent) -> Option<&str> {
        self.project_ids_by_token
            .get(&event.token)
            .map(String::as_str)
    }

    pub fn historical_migration(&self) -> bool {
        self.historical_migration
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The next byte a consumer has not incorporated yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WalCursor {
    pub segment: u64,
    pub byte_offset: u64,
}

impl WalCursor {
    pub const fn new(segment: u64, byte_offset: u64) -> Self {
        Self {
            segment,
            byte_offset,
        }
    }

    pub const fn origin() -> Self {
        Self::new(1, 0)
    }
}

/// Exact half-open byte range occupied by one durable record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalSpan {
    pub start: WalCursor,
    pub end: WalCursor,
    pub framed_bytes: u64,
}

/// WAL-local receipt returned only after the record's fsync completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalReceipt {
    pub span: WalSpan,
    pub event_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct WalConfig {
    pub segment_target_bytes: u64,
    pub max_record_bytes: usize,
}

impl WalConfig {
    pub fn new(segment_target_bytes: u64, max_record_bytes: usize) -> Result<Self, WalError> {
        if segment_target_bytes == 0
            || max_record_bytes == 0
            || max_record_bytes > u32::MAX as usize
        {
            return Err(WalError::InvalidConfig);
        }
        Ok(Self {
            segment_target_bytes,
            max_record_bytes,
        })
    }
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            segment_target_bytes: 64 * 1024 * 1024,
            max_record_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovery {
    pub truncated_tail: bool,
}

#[derive(Debug)]
pub enum WalError {
    Io(std::io::Error),
    Serialization(serde_json::Error),
    InvalidConfig,
    EmptyBatch,
    InvalidProjectBinding,
    Poisoned,
    WriterLocked,
    RecordTooLarge {
        actual: usize,
        maximum: usize,
    },
    Corruption {
        segment: u64,
        byte_offset: u64,
        reason: &'static str,
    },
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "WAL I/O error: {error}"),
            Self::Serialization(error) => write!(f, "WAL serialization error: {error}"),
            Self::InvalidConfig => f.write_str("invalid WAL configuration"),
            Self::EmptyBatch => f.write_str("a WAL batch must contain at least one event"),
            Self::InvalidProjectBinding => {
                f.write_str("every WAL event must have a non-empty authorized project binding")
            }
            Self::Poisoned => f.write_str("WAL writer is poisoned after an I/O failure"),
            Self::WriterLocked => f.write_str("WAL directory already has an active writer"),
            Self::RecordTooLarge { actual, maximum } => {
                write!(f, "WAL record is {actual} bytes; maximum is {maximum}")
            }
            Self::Corruption {
                segment,
                byte_offset,
                reason,
            } => write!(
                f,
                "WAL corruption in segment {segment} at byte {byte_offset}: {reason}"
            ),
        }
    }
}

impl std::error::Error for WalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WalError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for WalError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}

struct ActiveSegment {
    sequence: u64,
    path: PathBuf,
    file: File,
    bytes: u64,
}

/// Single-writer byte WAL. `append` returns only after the record is fsynced.
pub struct WriteAheadLog {
    directory: PathBuf,
    config: WalConfig,
    active: ActiveSegment,
    poisoned: bool,
    _writer_lock: File,
}

impl WriteAheadLog {
    pub fn open(
        directory: impl AsRef<Path>,
        config: WalConfig,
    ) -> Result<(Self, Recovery), WalError> {
        let directory = directory.as_ref().to_path_buf();
        create_directory_durable(&directory)?;
        let writer_lock = acquire_writer_lock(&directory)?;
        let layout = discover_segment_layout(&directory)?;
        for (sequence, path) in &layout.sealed {
            scan_segment(path, *sequence, config.max_record_bytes, false)?;
        }
        let last_sealed = layout.sealed.last().map(|(sequence, _)| *sequence);
        let (active, truncated_tail) = match layout.active.as_ref() {
            Some((sequence, path)) => {
                let scan = scan_segment(path, *sequence, config.max_record_bytes, true)?;
                let mut file = OpenOptions::new().read(true).write(true).open(path)?;
                file.seek(SeekFrom::Start(scan.valid_bytes))?;
                (
                    ActiveSegment {
                        sequence: *sequence,
                        path: path.clone(),
                        file,
                        bytes: scan.valid_bytes,
                    },
                    scan.truncated_tail,
                )
            }
            None => {
                let sequence = match last_sealed {
                    Some(sequence) => sequence.checked_add(1).ok_or(WalError::InvalidConfig)?,
                    None => 1,
                };
                (open_new_active(&directory, sequence)?, false)
            }
        };
        Ok((
            Self {
                directory,
                config,
                active,
                poisoned: false,
                _writer_lock: writer_lock,
            },
            Recovery { truncated_tail },
        ))
    }

    /// Serialize, frame, write, and fsync one batch before returning its
    /// durable byte span.
    pub fn append(&mut self, batch: CapturedBatch) -> Result<WalReceipt, WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        if batch.events.is_empty() {
            return Err(WalError::EmptyBatch);
        }
        let event_count = batch.event_count();
        let payload = serde_json::to_vec(&batch)?;
        if payload.len() > self.config.max_record_bytes {
            return Err(WalError::RecordTooLarge {
                actual: payload.len(),
                maximum: self.config.max_record_bytes,
            });
        }
        let framed_bytes = (RECORD_HEADER_BYTES + payload.len()) as u64;
        if self.active.bytes > 0
            && self.active.bytes.saturating_add(framed_bytes) > self.config.segment_target_bytes
        {
            self.seal_nonempty()?;
        }

        let start = WalCursor::new(self.active.sequence, self.active.bytes);
        let frame = encode_frame(&payload);
        if let Err(error) = self.active.file.write_all(&frame) {
            self.poisoned = true;
            return Err(error.into());
        }
        if let Err(error) = self.active.file.sync_all() {
            self.poisoned = true;
            return Err(error.into());
        }
        self.active.bytes += framed_bytes;
        Ok(WalReceipt {
            span: WalSpan {
                start,
                end: WalCursor::new(self.active.sequence, self.active.bytes),
                framed_bytes,
            },
            event_count,
        })
    }

    /// Make the current non-empty segment immutable and start a new active
    /// segment. An empty active segment is left in place.
    pub fn seal(&mut self) -> Result<WalCursor, WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        if self.active.bytes > 0 {
            self.seal_nonempty()?;
        }
        Ok(WalCursor::new(self.active.sequence, 0))
    }

    pub fn sealed_records_from(&self, cursor: WalCursor) -> Result<SealedRecords, WalError> {
        SealedRecords::open(&self.directory, cursor, self.config.max_record_bytes)
    }

    /// Stream at most `max_bytes` of whole sealed records from `cursor`.
    /// The first record is yielded even when it alone exceeds the budget, so
    /// a legal record can never permanently block publication.
    pub fn read_window(
        &self,
        cursor: WalCursor,
        max_bytes: u64,
    ) -> Result<PublicationWindow, WalError> {
        if max_bytes == 0 {
            return Err(WalError::InvalidConfig);
        }
        Ok(PublicationWindow {
            records: self.sealed_records_from(cursor)?,
            checkpoint: cursor,
            max_bytes,
            framed_bytes: 0,
            finished: false,
        })
    }

    /// Reclaim sealed segments covered by a committed checkpoint. A segment
    /// equal to the checkpoint segment is removed only when the checkpoint is
    /// exactly its validated EOF; a partial-segment checkpoint never removes
    /// that segment.
    pub fn reclaim_through(&self, checkpoint: WalCursor) -> Result<Vec<u64>, WalError> {
        let layout = discover_segment_layout(&self.directory)?;
        let validated = validate_cursor(&layout, checkpoint, self.config.max_record_bytes)?;
        let mut reclaim = Vec::new();
        for (sequence, path) in layout.sealed {
            if sequence > checkpoint.segment {
                break;
            }
            let covered = sequence < checkpoint.segment
                || (sequence == checkpoint.segment
                    && checkpoint.byte_offset == validated.valid_bytes);
            if covered {
                if sequence != checkpoint.segment {
                    scan_segment(&path, sequence, self.config.max_record_bytes, false)?;
                }
                reclaim.push((sequence, path));
            }
        }
        for (_, path) in &reclaim {
            std::fs::remove_file(path)?;
        }
        if !reclaim.is_empty() {
            sync_directory(&self.directory)?;
        }
        Ok(reclaim.into_iter().map(|(sequence, _)| sequence).collect())
    }

    fn seal_nonempty(&mut self) -> Result<(), WalError> {
        if let Err(error) = self.active.file.sync_all() {
            self.poisoned = true;
            return Err(error.into());
        }
        let sealed = sealed_path(&self.directory, self.active.sequence);
        if let Err(error) = std::fs::rename(&self.active.path, sealed) {
            self.poisoned = true;
            return Err(error.into());
        }
        if let Err(error) = sync_directory(&self.directory) {
            self.poisoned = true;
            return Err(error);
        }
        let next = self
            .active
            .sequence
            .checked_add(1)
            .ok_or(WalError::InvalidConfig)?;
        self.active = match open_new_active(&self.directory, next) {
            Ok(active) => active,
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        };
        Ok(())
    }
}

#[derive(Debug)]
pub struct WalRecord {
    pub batch: CapturedBatch,
    pub span: WalSpan,
}

/// Lazy iterator over sealed records. It allocates at most one bounded record
/// payload and never loads a whole segment into memory.
#[derive(Debug)]
pub struct SealedRecords {
    segments: Vec<(u64, PathBuf)>,
    segment_index: usize,
    file: Option<File>,
    next: WalCursor,
    terminal_cursor: Option<WalCursor>,
    max_record_bytes: usize,
    finished: bool,
}

impl SealedRecords {
    fn open(
        directory: &Path,
        cursor: WalCursor,
        max_record_bytes: usize,
    ) -> Result<Self, WalError> {
        let layout = discover_segment_layout(directory)?;
        validate_cursor(&layout, cursor, max_record_bytes)?;
        let terminal_cursor = layout
            .active
            .as_ref()
            .map(|(sequence, _)| WalCursor::new(*sequence, 0));
        let segments = layout
            .sealed
            .into_iter()
            .filter(|(sequence, _)| *sequence >= cursor.segment)
            .collect();
        Ok(Self {
            segments,
            segment_index: 0,
            file: None,
            next: cursor,
            terminal_cursor,
            max_record_bytes,
            finished: false,
        })
    }

    pub fn next_cursor(&self) -> WalCursor {
        self.next
    }

    fn next_record(&mut self) -> Result<Option<WalRecord>, WalError> {
        loop {
            let Some((sequence, path)) = self.segments.get(self.segment_index) else {
                return Ok(None);
            };
            if self.file.is_none() {
                let mut file = File::open(path)?;
                let offset = if *sequence == self.next.segment {
                    self.next.byte_offset
                } else {
                    0
                };
                file.seek(SeekFrom::Start(offset))?;
                self.next = WalCursor::new(*sequence, offset);
                self.file = Some(file);
            }

            let Some(file) = self.file.as_mut() else {
                continue;
            };
            let record_start = self.next;
            match read_record(
                file,
                *sequence,
                record_start.byte_offset,
                self.max_record_bytes,
            )? {
                Some((batch, framed_bytes)) => {
                    let record_end = WalCursor::new(
                        record_start.segment,
                        record_start.byte_offset + framed_bytes,
                    );
                    let reached_eof = file.stream_position()? == file.metadata()?.len();
                    if reached_eof {
                        self.segment_index += 1;
                        self.file = None;
                        self.next = self
                            .segments
                            .get(self.segment_index)
                            .map(|(next_sequence, _)| WalCursor::new(*next_sequence, 0))
                            .or(self.terminal_cursor)
                            .unwrap_or(record_end);
                    } else {
                        self.next = record_end;
                    }
                    return Ok(Some(WalRecord {
                        span: WalSpan {
                            start: record_start,
                            end: record_end,
                            framed_bytes,
                        },
                        batch,
                    }));
                }
                None => {
                    self.segment_index += 1;
                    self.file = None;
                    if let Some((next_sequence, _)) = self.segments.get(self.segment_index) {
                        self.next = WalCursor::new(*next_sequence, 0);
                    } else if let Some(terminal_cursor) = self.terminal_cursor {
                        self.next = terminal_cursor;
                    }
                }
            }
        }
    }
}

impl Iterator for SealedRecords {
    type Item = Result<WalRecord, WalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.next_record() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

/// Lazy byte-bounded view over sealed WAL records.
pub struct PublicationWindow {
    records: SealedRecords,
    checkpoint: WalCursor,
    max_bytes: u64,
    framed_bytes: u64,
    finished: bool,
}

impl PublicationWindow {
    /// Normalized checkpoint immediately after the last yielded record.
    pub fn next_cursor(&self) -> WalCursor {
        self.checkpoint
    }

    pub fn framed_bytes(&self) -> u64 {
        self.framed_bytes
    }
}

impl Iterator for PublicationWindow {
    type Item = Result<WalRecord, WalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let record = match self.records.next() {
            Some(Ok(record)) => record,
            Some(Err(error)) => {
                self.finished = true;
                return Some(Err(error));
            }
            None => {
                self.checkpoint = self.records.next_cursor();
                self.finished = true;
                return None;
            }
        };
        if self.framed_bytes > 0
            && self.framed_bytes.saturating_add(record.span.framed_bytes) > self.max_bytes
        {
            self.finished = true;
            return None;
        }
        self.framed_bytes += record.span.framed_bytes;
        self.checkpoint = self.records.next_cursor();
        Some(Ok(record))
    }
}

fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(RECORD_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&RECORD_MAGIC);
    frame.push(RECORD_VERSION);
    frame.extend_from_slice(&[0; 3]);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn read_record(
    file: &mut File,
    segment: u64,
    byte_offset: u64,
    max_record_bytes: usize,
) -> Result<Option<(CapturedBatch, u64)>, WalError> {
    let mut header = [0_u8; RECORD_HEADER_BYTES];
    let read = read_up_to(file, &mut header)?;
    if read == 0 {
        return Ok(None);
    }
    if read != RECORD_HEADER_BYTES {
        return Err(corruption(segment, byte_offset, "incomplete record header"));
    }
    if header[..4] != RECORD_MAGIC {
        return Err(corruption(segment, byte_offset, "invalid record magic"));
    }
    if header[4] != RECORD_VERSION {
        return Err(corruption(
            segment,
            byte_offset,
            "unsupported record version",
        ));
    }
    if header[5..8] != [0, 0, 0] {
        return Err(corruption(
            segment,
            byte_offset,
            "non-zero reserved header bytes",
        ));
    }
    let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length > max_record_bytes {
        return Err(corruption(
            segment,
            byte_offset,
            "record length exceeds limit",
        ));
    }
    let expected_crc = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    let mut payload = vec![0; length];
    if read_up_to(file, &mut payload)? != length {
        return Err(corruption(
            segment,
            byte_offset,
            "incomplete record payload",
        ));
    }
    if crc32fast::hash(&payload) != expected_crc {
        return Err(corruption(segment, byte_offset, "record checksum mismatch"));
    }
    let batch: CapturedBatch = serde_json::from_slice(&payload)
        .map_err(|_| corruption(segment, byte_offset, "invalid captured batch payload"))?;
    if batch.events.is_empty() {
        return Err(corruption(segment, byte_offset, "empty captured batch"));
    }
    Ok(Some((batch, (RECORD_HEADER_BYTES + length) as u64)))
}

struct SegmentScan {
    valid_bytes: u64,
    truncated_tail: bool,
}

/// Validate a segment without loading it. Only an active segment may discard
/// an incomplete final header or payload. A complete frame with a bad header,
/// checksum, or payload is corruption even when it is the final frame.
fn scan_segment(
    path: &Path,
    sequence: u64,
    max_record_bytes: usize,
    repair_final_tail: bool,
) -> Result<SegmentScan, WalError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(repair_final_tail)
        .open(path)?;
    let file_bytes = file.metadata()?.len();
    let mut offset = 0_u64;
    let mut truncated_tail = false;
    while offset < file_bytes {
        let remaining = file_bytes - offset;
        if remaining < RECORD_HEADER_BYTES as u64 {
            if repair_final_tail {
                truncated_tail = true;
                break;
            }
            return Err(corruption(sequence, offset, "incomplete record header"));
        }

        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; RECORD_HEADER_BYTES];
        file.read_exact(&mut header)?;
        validate_header(&header, sequence, offset)?;
        let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        if length > max_record_bytes {
            return Err(corruption(sequence, offset, "record length exceeds limit"));
        }
        let frame_bytes = (RECORD_HEADER_BYTES as u64)
            .checked_add(length as u64)
            .ok_or_else(|| corruption(sequence, offset, "record length overflow"))?;
        let frame_end = offset
            .checked_add(frame_bytes)
            .ok_or_else(|| corruption(sequence, offset, "record offset overflow"))?;
        if frame_end > file_bytes {
            if repair_final_tail {
                truncated_tail = true;
                break;
            }
            return Err(corruption(sequence, offset, "incomplete record payload"));
        }

        let expected_crc = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        let mut payload = vec![0; length];
        file.read_exact(&mut payload)?;
        if crc32fast::hash(&payload) != expected_crc {
            return Err(corruption(sequence, offset, "record checksum mismatch"));
        }
        let batch: CapturedBatch = serde_json::from_slice(&payload)
            .map_err(|_| corruption(sequence, offset, "invalid captured batch payload"))?;
        if batch.events.is_empty() {
            return Err(corruption(sequence, offset, "empty captured batch"));
        }
        offset = frame_end;
    }

    if truncated_tail {
        file.set_len(offset)?;
        file.sync_all()?;
    }
    Ok(SegmentScan {
        valid_bytes: offset,
        truncated_tail,
    })
}

fn validate_header(
    header: &[u8; RECORD_HEADER_BYTES],
    segment: u64,
    offset: u64,
) -> Result<(), WalError> {
    if header[..4] != RECORD_MAGIC {
        return Err(corruption(segment, offset, "invalid record magic"));
    }
    if header[4] != RECORD_VERSION {
        return Err(corruption(segment, offset, "unsupported record version"));
    }
    if header[5..8] != [0, 0, 0] {
        return Err(corruption(
            segment,
            offset,
            "non-zero reserved header bytes",
        ));
    }
    Ok(())
}

fn read_up_to(file: &mut File, destination: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < destination.len() {
        let read = file.read(&mut destination[total..])?;
        if read == 0 {
            break;
        }
        total += read;
    }
    Ok(total)
}

struct SegmentLayout {
    sealed: Vec<(u64, PathBuf)>,
    active: Option<(u64, PathBuf)>,
}

/// Discover the complete WAL layout and reject any ambiguity before a caller
/// reads, repairs, or removes a segment. The first sequence may be greater
/// than one after prefix reclamation; every sequence after it must be exactly
/// contiguous.
fn discover_segment_layout(directory: &Path) -> Result<SegmentLayout, WalError> {
    let sealed = list_segments(directory, SEALED_SUFFIX)?;
    let active_segments = list_segments(directory, OPEN_SUFFIX)?;
    if active_segments.len() > 1 {
        return Err(corruption(0, 0, "multiple active segments"));
    }
    let active = active_segments.into_iter().next();

    let mut sequences = sealed
        .iter()
        .map(|(sequence, _)| (*sequence, false))
        .chain(active.iter().map(|(sequence, _)| (*sequence, true)))
        .collect::<Vec<_>>();
    sequences.sort_by_key(|(sequence, _)| *sequence);

    if sequences
        .first()
        .is_some_and(|(sequence, _)| *sequence == 0)
    {
        return Err(corruption(0, 0, "segment sequence must be positive"));
    }
    for pair in sequences.windows(2) {
        let (previous, _) = pair[0];
        let (current, _) = pair[1];
        let Some(expected) = previous.checked_add(1) else {
            return Err(corruption(previous, 0, "segment sequence overflow"));
        };
        if current == previous {
            return Err(corruption(current, 0, "duplicate segment sequence"));
        }
        if current != expected {
            return Err(corruption(
                current,
                0,
                "missing WAL segment before this sequence",
            ));
        }
    }
    if let Some((active_sequence, _)) = active.as_ref() {
        if sequences
            .last()
            .is_some_and(|(last_sequence, _)| active_sequence != last_sequence)
        {
            return Err(corruption(
                *active_sequence,
                0,
                "active segment is not newest",
            ));
        }
    }

    Ok(SegmentLayout { sealed, active })
}

struct ValidatedCursor {
    valid_bytes: u64,
}

fn validate_cursor(
    layout: &SegmentLayout,
    cursor: WalCursor,
    max_record_bytes: usize,
) -> Result<ValidatedCursor, WalError> {
    let path = layout
        .sealed
        .iter()
        .find(|(sequence, _)| *sequence == cursor.segment)
        .map(|(_, path)| path)
        .or_else(|| {
            layout
                .active
                .as_ref()
                .filter(|(sequence, _)| *sequence == cursor.segment)
                .map(|(_, path)| path)
        })
        .ok_or_else(|| {
            corruption(
                cursor.segment,
                cursor.byte_offset,
                "cursor segment does not exist",
            )
        })?;
    let scan = scan_segment(path, cursor.segment, max_record_bytes, false)?;
    if cursor.byte_offset > scan.valid_bytes {
        return Err(corruption(
            cursor.segment,
            cursor.byte_offset,
            "cursor is beyond segment EOF",
        ));
    }
    if !is_record_boundary(path, cursor.segment, cursor.byte_offset, max_record_bytes)? {
        return Err(corruption(
            cursor.segment,
            cursor.byte_offset,
            "cursor is not on a record boundary",
        ));
    }
    Ok(ValidatedCursor {
        valid_bytes: scan.valid_bytes,
    })
}

/// `scan_segment` has already authenticated every frame. This second pass
/// reads headers only to prove that the requested cursor is one of their
/// exact boundaries without retaining a boundary table proportional to the
/// number of records.
fn is_record_boundary(
    path: &Path,
    sequence: u64,
    target: u64,
    max_record_bytes: usize,
) -> Result<bool, WalError> {
    if target == 0 {
        return Ok(true);
    }
    let mut file = File::open(path)?;
    let mut offset = 0_u64;
    while offset < target {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; RECORD_HEADER_BYTES];
        file.read_exact(&mut header)?;
        validate_header(&header, sequence, offset)?;
        let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        if length > max_record_bytes {
            return Err(corruption(sequence, offset, "record length exceeds limit"));
        }
        let frame_bytes = (RECORD_HEADER_BYTES as u64)
            .checked_add(length as u64)
            .ok_or_else(|| corruption(sequence, offset, "record length overflow"))?;
        offset = offset
            .checked_add(frame_bytes)
            .ok_or_else(|| corruption(sequence, offset, "record offset overflow"))?;
        if offset > target {
            return Ok(false);
        }
    }
    Ok(offset == target)
}

fn corruption(segment: u64, byte_offset: u64, reason: &'static str) -> WalError {
    WalError::Corruption {
        segment,
        byte_offset,
        reason,
    }
}

fn create_directory_durable(directory: &Path) -> Result<(), WalError> {
    std::fs::create_dir_all(directory)?;
    if let Some(parent) = directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        sync_directory(parent)?;
    }
    sync_directory(directory)
}

fn acquire_writer_lock(directory: &Path) -> Result<File, WalError> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(directory.join(WRITER_LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(WalError::WriterLocked),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn sync_directory(directory: &Path) -> Result<(), WalError> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn open_new_active(directory: &Path, sequence: u64) -> Result<ActiveSegment, WalError> {
    let path = active_path(directory, sequence);
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    sync_directory(directory)?;
    Ok(ActiveSegment {
        sequence,
        path,
        file,
        bytes: 0,
    })
}

fn active_path(directory: &Path, sequence: u64) -> PathBuf {
    directory.join(format!("{sequence:016}{OPEN_SUFFIX}"))
}

fn sealed_path(directory: &Path, sequence: u64) -> PathBuf {
    directory.join(format!("{sequence:016}{SEALED_SUFFIX}"))
}

fn list_segments(directory: &Path, suffix: &str) -> Result<Vec<(u64, PathBuf)>, WalError> {
    let mut segments = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(sequence) = name
            .strip_suffix(suffix)
            .filter(|digits| digits.len() == 16)
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        segments.push((sequence, path));
    }
    segments.sort_by_key(|(sequence, _)| *sequence);
    Ok(segments)
}

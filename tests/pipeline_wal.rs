mod capture {
    pub mod event {
        pub use hoglet::capture::event::CapturedEvent;
    }
}

#[path = "../src/pipeline/wal.rs"]
mod pipeline_wal;

use chrono::{TimeZone, Utc};
use pipeline_wal::{CapturedBatch, WalConfig, WalCursor, WalError, WriteAheadLog};
use serde_json::{Map, Value};
use std::fs::OpenOptions;
use uuid::Uuid;

fn event(sequence: u128, payload_len: usize) -> capture::event::CapturedEvent {
    let mut properties = Map::new();
    properties.insert("payload".into(), Value::String("x".repeat(payload_len)));
    capture::event::CapturedEvent {
        uuid: Uuid::from_u128(sequence),
        event: "pageview".into(),
        distinct_id: format!("person-{sequence}"),
        token: "phc_testproject".into(),
        timestamp: Utc
            .timestamp_opt(sequence as i64, 0)
            .single()
            .expect("test timestamp is valid"),
        properties,
    }
}

#[test]
fn durable_batch_round_trips_from_a_sealed_segment() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(4 * 1024, 4 * 1024).expect("valid WAL config");
    let (mut wal, recovery) =
        WriteAheadLog::open(dir.path(), config).expect("WAL opens in an empty directory");
    assert!(!recovery.truncated_tail);

    let batch = CapturedBatch::new(vec![event(1, 12)]).expect("non-empty batch");
    let expected = serde_json::to_value(&batch).expect("batch serializes");
    let receipt = wal.append(batch).expect("append is durable");
    assert_eq!(receipt.span.start, WalCursor::new(1, 0));
    assert_eq!(receipt.span.end.byte_offset, receipt.span.framed_bytes);
    assert_eq!(receipt.event_count, 1);

    wal.seal().expect("active segment seals");
    let mut records = wal
        .sealed_records_from(WalCursor::new(1, 0))
        .expect("sealed records open");
    let record = records
        .next()
        .expect("one record exists")
        .expect("record is valid");
    assert_eq!(serde_json::to_value(record.batch).unwrap(), expected);
    assert_eq!(record.span, receipt.span);
    assert!(records.next().is_none());
}

#[test]
fn publication_window_stops_between_records_and_allows_one_oversize_record() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let first = wal
        .append(CapturedBatch::new(vec![event(1, 80)]).unwrap())
        .expect("first batch is durable");
    let second = wal
        .append(CapturedBatch::new(vec![event(2, 80)]).unwrap())
        .expect("second batch is durable");
    wal.seal().expect("segment seals");

    let mut window = wal
        .read_window(WalCursor::origin(), first.span.framed_bytes + 1)
        .expect("publication window opens");
    let record = window.next().unwrap().expect("first record is readable");
    assert_eq!(record.span, first.span);
    assert!(window.next().is_none());
    assert_eq!(window.next_cursor(), first.span.end);
    assert_eq!(window.framed_bytes(), first.span.framed_bytes);

    let mut remainder = wal
        .read_window(window.next_cursor(), 1)
        .expect("oversize publication window opens");
    let record = remainder
        .next()
        .unwrap()
        .expect("one oversize record is allowed");
    assert_eq!(record.span, second.span);
    assert!(remainder.next().is_none());
    assert_eq!(
        remainder.next_cursor(),
        WalCursor::new(second.span.end.segment + 1, 0),
        "a sealed EOF checkpoint normalizes to the active segment origin"
    );
}

#[test]
fn reopen_truncates_only_a_torn_final_record_and_resumes_at_its_start() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let torn = wal
        .append(CapturedBatch::new(vec![event(1, 100)]).unwrap())
        .expect("batch is durable before simulated disk tear");
    drop(wal);

    let active_path = dir.path().join("0000000000000001.open");
    let active = OpenOptions::new()
        .write(true)
        .open(&active_path)
        .expect("active segment exists");
    active
        .set_len(torn.span.end.byte_offset - 3)
        .expect("simulate a torn final payload");
    active.sync_all().expect("simulated tear reaches disk");

    let (mut recovered, recovery) =
        WriteAheadLog::open(dir.path(), config).expect("torn final record is recoverable");
    assert!(recovery.truncated_tail);
    let replacement = recovered
        .append(CapturedBatch::new(vec![event(2, 1)]).unwrap())
        .expect("writer resumes at valid prefix");
    assert_eq!(replacement.span.start, WalCursor::origin());
    recovered.seal().expect("replacement segment seals");

    let batches = recovered
        .sealed_records_from(WalCursor::origin())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .expect("recovered WAL is readable");
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].batch.events[0].uuid, Uuid::from_u128(2));
}

#[test]
fn publication_cursor_normalizes_to_the_next_segment_after_a_roll() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(1, 8 * 1024).expect("one record may exceed segment target");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let first = wal
        .append(CapturedBatch::new(vec![event(1, 20)]).unwrap())
        .expect("first batch is durable");
    let second = wal
        .append(CapturedBatch::new(vec![event(2, 20)]).unwrap())
        .expect("second batch rolls to a new segment");
    assert_eq!(first.span.start.segment, 1);
    assert_eq!(second.span.start.segment, 2);
    wal.seal().expect("second segment seals");

    let mut window = wal
        .read_window(WalCursor::origin(), first.span.framed_bytes)
        .expect("publication window opens");
    assert_eq!(window.next().unwrap().unwrap().span, first.span);
    assert!(window.next().is_none());
    assert_eq!(window.next_cursor(), WalCursor::new(2, 0));
}

#[test]
fn reopen_rejects_corruption_before_another_complete_record() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let first = wal
        .append(CapturedBatch::new(vec![event(1, 20)]).unwrap())
        .expect("first batch is durable");
    let second = wal
        .append(CapturedBatch::new(vec![event(2, 20)]).unwrap())
        .expect("second batch is durable");
    drop(wal);

    let path = dir.path().join("0000000000000001.open");
    let mut bytes = std::fs::read(&path).expect("active segment is readable");
    bytes[(first.span.end.byte_offset - 1) as usize] ^= 0xff;
    std::fs::write(&path, bytes).expect("inject an interior checksum error");

    let error = match WriteAheadLog::open(dir.path(), config) {
        Ok(_) => panic!("interior corruption must fail recovery"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("checksum mismatch"));
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        second.span.end.byte_offset
    );
}

#[test]
fn record_frame_is_versioned_length_delimited_and_checksummed() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let receipt = wal
        .append(CapturedBatch::new(vec![event(7, 3)]).unwrap())
        .expect("batch is durable");
    wal.seal().expect("segment seals");

    let frame =
        std::fs::read(dir.path().join("0000000000000001.wal")).expect("sealed frame is readable");
    assert_eq!(&frame[..4], b"HGLW");
    assert_eq!(frame[4], 1);
    assert_eq!(&frame[5..8], &[0, 0, 0]);
    let payload_length = u32::from_le_bytes(frame[8..12].try_into().unwrap()) as usize;
    assert_eq!(payload_length + 16, frame.len());
    let checksum = u32::from_le_bytes(frame[12..16].try_into().unwrap());
    assert_eq!(checksum, crc32fast::hash(&frame[16..]));
    assert_eq!(receipt.span.framed_bytes, frame.len() as u64);
}

#[test]
fn record_size_limit_is_checked_before_allocating_or_writing_payload() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(4 * 1024, 64).expect("small test limit is valid");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let error = wal
        .append(CapturedBatch::new(vec![event(1, 128)]).unwrap())
        .expect_err("oversize serialized batch is rejected");
    assert!(matches!(
        error,
        WalError::RecordTooLarge { maximum: 64, .. }
    ));
    assert_eq!(
        std::fs::metadata(dir.path().join("0000000000000001.open"))
            .unwrap()
            .len(),
        0
    );
    drop(wal);

    let mut malicious_header = Vec::from(*b"HGLW");
    malicious_header.extend_from_slice(&[1, 0, 0, 0]);
    malicious_header.extend_from_slice(&65_u32.to_le_bytes());
    malicious_header.extend_from_slice(&0_u32.to_le_bytes());
    std::fs::write(dir.path().join("0000000000000001.open"), &malicious_header)
        .expect("write hostile length header");
    let error = match WriteAheadLog::open(dir.path(), config) {
        Ok(_) => panic!("hostile record length must fail recovery"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("record length exceeds limit"));
    assert_eq!(
        std::fs::metadata(dir.path().join("0000000000000001.open"))
            .unwrap()
            .len(),
        16
    );
}

#[test]
fn reclamation_removes_a_segment_only_at_its_valid_eof_checkpoint() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let first = wal
        .append(CapturedBatch::new(vec![event(1, 10)]).unwrap())
        .expect("first batch is durable");
    let second = wal
        .append(CapturedBatch::new(vec![event(2, 10)]).unwrap())
        .expect("second batch is durable");
    wal.seal().expect("segment seals");

    let reclaimed = wal
        .reclaim_through(first.span.end)
        .expect("partial checkpoint is valid but not reclaimable");
    assert!(reclaimed.is_empty());
    assert!(dir.path().join("0000000000000001.wal").exists());

    let reclaimed = wal
        .reclaim_through(second.span.end)
        .expect("EOF checkpoint is reclaimable");
    assert_eq!(reclaimed, vec![1]);
    assert!(!dir.path().join("0000000000000001.wal").exists());
}

#[test]
fn reopen_rejects_an_interior_segment_gap_but_accepts_a_reclaimed_prefix() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(1, 8 * 1024).expect("one record per segment");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let first = wal
        .append(CapturedBatch::new(vec![event(1, 10)]).unwrap())
        .expect("first batch is durable");
    wal.append(CapturedBatch::new(vec![event(2, 10)]).unwrap())
        .expect("second batch rolls the WAL");
    wal.append(CapturedBatch::new(vec![event(3, 10)]).unwrap())
        .expect("third batch rolls the WAL again");
    wal.seal().expect("third segment seals");

    wal.reclaim_through(first.span.end)
        .expect("the first segment is explicitly reclaimed");
    drop(wal);
    let (wal, _) = WriteAheadLog::open(dir.path(), config)
        .expect("a contiguous WAL may start after a reclaimed prefix");
    drop(wal);

    std::fs::remove_file(dir.path().join("0000000000000003.wal"))
        .expect("simulate loss of an interior segment");
    let error = match WriteAheadLog::open(dir.path(), config) {
        Ok(_) => panic!("an interior segment gap must fail recovery"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("missing WAL segment"));
}

#[test]
fn starting_cursor_beyond_segment_eof_is_rejected_before_iteration() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    let receipt = wal
        .append(CapturedBatch::new(vec![event(1, 10)]).unwrap())
        .expect("batch is durable");
    wal.seal().expect("segment seals");

    let error = wal
        .sealed_records_from(WalCursor::new(1, receipt.span.end.byte_offset + 1))
        .expect_err("a cursor beyond EOF is invalid");
    assert!(error.to_string().contains("beyond segment EOF"));
}

#[test]
fn starting_cursor_in_the_middle_of_a_frame_is_rejected_before_iteration() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    wal.append(CapturedBatch::new(vec![event(1, 10)]).unwrap())
        .expect("batch is durable");
    wal.seal().expect("segment seals");

    let error = wal
        .sealed_records_from(WalCursor::new(1, 1))
        .expect_err("a cursor inside a frame is invalid");
    assert!(error.to_string().contains("record boundary"));
}

#[test]
fn forged_reclamation_checkpoint_never_deletes_a_valid_prefix() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(1, 8 * 1024).expect("one record per segment");
    let (mut wal, _) = WriteAheadLog::open(dir.path(), config).expect("WAL opens");
    wal.append(CapturedBatch::new(vec![event(1, 10)]).unwrap())
        .expect("first batch is durable");
    wal.append(CapturedBatch::new(vec![event(2, 10)]).unwrap())
        .expect("second batch rolls the WAL");
    wal.seal().expect("second segment seals");
    let first_path = dir.path().join("0000000000000001.wal");
    let second_path = dir.path().join("0000000000000002.wal");

    let error = wal
        .reclaim_through(WalCursor::new(99, 0))
        .expect_err("a checkpoint for a missing segment is forged");
    assert!(error.to_string().contains("does not exist"));
    assert!(first_path.exists());
    assert!(second_path.exists());

    let error = wal
        .reclaim_through(WalCursor::new(2, 1))
        .expect_err("a checkpoint inside a frame is forged");
    assert!(error.to_string().contains("record boundary"));
    assert!(
        first_path.exists(),
        "validation must precede every deletion"
    );
    assert!(second_path.exists());
}

#[test]
fn only_one_writer_can_open_a_wal_directory() {
    let dir = tempfile::tempdir().expect("temporary WAL directory");
    let config = WalConfig::new(16 * 1024, 8 * 1024).expect("valid WAL config");
    let (wal, _) = WriteAheadLog::open(dir.path(), config).expect("first writer opens");

    let error = match WriteAheadLog::open(dir.path(), config) {
        Ok(_) => panic!("a second writer must not open the same WAL"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("already has an active writer"));

    drop(wal);
    WriteAheadLog::open(dir.path(), config).expect("the lock is released with the writer");
}

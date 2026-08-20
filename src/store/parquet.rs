//! Parquet encoding of captured events.
//!
//! Schema v1: fixed columns for the fields every query touches, properties
//! as a JSON string column. DuckDB reads these files directly; the schema is
//! a wire-adjacent contract — additive changes only.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use chrono::{DateTime, Utc};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::properties::WriterProperties;
use uuid::Uuid;

use crate::capture::event::CapturedEvent;

/// On-disk event schema version, stamped into every Parquet file's key-value
/// metadata (spec/README.md "Format evolution"). Bump only on a breaking change; the
/// reader must union versions. Additive columns do not bump this.
pub const SCHEMA_VERSION: &str = "1";

pub fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("uuid", DataType::Utf8, false),
        Field::new("event", DataType::Utf8, false),
        Field::new("distinct_id", DataType::Utf8, false),
        Field::new("token", DataType::Utf8, false),
        Field::new(
            "timestamp",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("properties", DataType::Utf8, false),
    ]))
}

fn to_record_batch(events: &[CapturedEvent]) -> Result<RecordBatch, arrow::error::ArrowError> {
    let uuids = StringArray::from_iter_values(events.iter().map(|e| e.uuid.to_string()));
    let names = StringArray::from_iter_values(events.iter().map(|e| e.event.as_str()));
    let distinct_ids = StringArray::from_iter_values(events.iter().map(|e| e.distinct_id.as_str()));
    let tokens = StringArray::from_iter_values(events.iter().map(|e| e.token.as_str()));
    let timestamps = TimestampMicrosecondArray::from_iter_values(
        events.iter().map(|e| e.timestamp.timestamp_micros()),
    )
    .with_timezone("UTC");
    let properties = StringArray::from_iter_values(
        events
            .iter()
            .map(|e| serde_json::Value::Object(e.properties.clone()).to_string()),
    );

    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(uuids) as ArrayRef,
            Arc::new(names),
            Arc::new(distinct_ids),
            Arc::new(tokens),
            Arc::new(timestamps),
            Arc::new(properties),
        ],
    )
}

/// Write events to `path` and fsync it. Caller handles atomic publish
/// (tmp + rename).
pub fn write_file(events: &[CapturedEvent], path: &Path) -> std::io::Result<()> {
    let batch = to_record_batch(events).map_err(std::io::Error::other)?;
    let file = File::create(path)?;
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
            "hoglet_schema_version".to_string(),
            SCHEMA_VERSION.to_string(),
        )]))
        .build();
    let mut writer =
        ArrowWriter::try_new(file, schema(), Some(props)).map_err(std::io::Error::other)?;
    writer.write(&batch).map_err(std::io::Error::other)?;
    let file = writer.into_inner().map_err(std::io::Error::other)?;
    file.sync_all()
}

/// Read a Parquet file back into events (compaction + tests; queries go
/// through DuckDB).
pub fn read_file(path: &Path) -> std::io::Result<Vec<CapturedEvent>> {
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(std::io::Error::other)?
        .build()
        .map_err(std::io::Error::other)?;

    let mut events = Vec::new();
    for batch in reader {
        let batch = batch.map_err(std::io::Error::other)?;
        let col = |name: &str| -> &StringArray {
            batch
                .column_by_name(name)
                .expect("schema column")
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("utf8 column")
        };
        let uuids = col("uuid");
        let names = col("event");
        let distinct_ids = col("distinct_id");
        let tokens = col("token");
        let properties = col("properties");
        let timestamps = batch
            .column_by_name("timestamp")
            .expect("schema column")
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .expect("timestamp column");

        for i in 0..batch.num_rows() {
            events.push(CapturedEvent {
                uuid: Uuid::parse_str(uuids.value(i))
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
                event: names.value(i).to_string(),
                distinct_id: distinct_ids.value(i).to_string(),
                token: tokens.value(i).to_string(),
                timestamp: DateTime::<Utc>::from_timestamp_micros(timestamps.value(i)).ok_or_else(
                    || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad timestamp"),
                )?,
                properties: match serde_json::from_str(properties.value(i)) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "properties not a JSON object",
                        ));
                    }
                },
            });
        }
    }
    Ok(events)
}

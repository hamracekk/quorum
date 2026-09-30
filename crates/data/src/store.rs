use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, RecordBatch, TimestampMillisecondArray};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use chrono::DateTime;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use trader_core::{Bar, Series};

use crate::{DataError, Result};

/// Directory of candle files: `<root>/<SYMBOL>-<interval>.parquet`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, symbol: &str, interval: &str) -> PathBuf {
        self.root.join(format!("{symbol}-{interval}.parquet"))
    }

    /// Stored candles, or `None` if nothing has been fetched for this pair yet.
    pub fn load(&self, symbol: &str, interval: &str) -> Result<Option<Series>> {
        let path = self.path(symbol, interval);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Series::new(symbol, read_parquet(&path)?)))
    }

    pub fn save(&self, series: &Series, interval: &str) -> Result<()> {
        write_parquet(&self.path(&series.symbol, interval), &series.bars)
    }
}

fn schema() -> Arc<Schema> {
    let price = |name| Field::new(name, DataType::Float64, false);
    Arc::new(Schema::new(vec![
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        price("open"),
        price("high"),
        price("low"),
        price("close"),
        price("volume"),
    ]))
}

/// Write `bars` atomically: to a temp file first, then renamed over `path`.
pub fn write_parquet(path: &Path, bars: &[Bar]) -> Result<()> {
    let io = |source| DataError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io)?;
    }

    let schema = schema();
    let ts = TimestampMillisecondArray::from(
        bars.iter()
            .map(|b| b.ts.timestamp_millis())
            .collect::<Vec<_>>(),
    )
    .with_timezone("UTC");
    let column = |field: fn(&Bar) -> f64| -> ArrayRef {
        Arc::new(Float64Array::from(
            bars.iter().map(field).collect::<Vec<_>>(),
        ))
    };
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(ts),
            column(|b| b.open),
            column(|b| b.high),
            column(|b| b.low),
            column(|b| b.close),
            column(|b| b.volume),
        ],
    )?;

    let tmp = path.with_extension("parquet.tmp");
    let file = File::create(&tmp).map_err(io)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
    writer.write(&batch)?;
    writer.close()?;
    fs::rename(&tmp, path).map_err(io)
}

/// Read candles written by [`write_parquet`], verifying they are sorted and unique.
pub fn read_parquet(path: &Path) -> Result<Vec<Bar>> {
    let corrupt = |reason: String| DataError::Corrupt {
        path: path.to_path_buf(),
        reason,
    };
    let file = File::open(path).map_err(|source| DataError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;

    let mut bars = Vec::new();
    for batch in reader {
        let batch = batch?;
        let ts = column::<TimestampMillisecondArray>(&batch, "ts").map_err(corrupt)?;
        let [open, high, low, close, volume] = ["open", "high", "low", "close", "volume"]
            .map(|name| column::<Float64Array>(&batch, name));
        let (open, high, low, close, volume) = (
            open.map_err(corrupt)?,
            high.map_err(corrupt)?,
            low.map_err(corrupt)?,
            close.map_err(corrupt)?,
            volume.map_err(corrupt)?,
        );
        for i in 0..batch.num_rows() {
            let millis = ts.value(i);
            bars.push(Bar {
                ts: DateTime::from_timestamp_millis(millis)
                    .ok_or_else(|| corrupt(format!("invalid timestamp {millis}")))?,
                open: open.value(i),
                high: high.value(i),
                low: low.value(i),
                close: close.value(i),
                volume: volume.value(i),
            });
        }
    }

    if !bars.windows(2).all(|w| w[0].ts < w[1].ts) {
        return Err(corrupt("bars are unsorted or duplicated".into()));
    }
    Ok(bars)
}

fn column<'a, T: 'static>(
    batch: &'a RecordBatch,
    name: &str,
) -> std::result::Result<&'a T, String> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<T>())
        .ok_or_else(|| format!("missing or mistyped column `{name}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(hour: i64, close: f64) -> Bar {
        Bar {
            ts: DateTime::from_timestamp(hour * 3_600, 0).expect("valid timestamp"),
            open: close - 1.0,
            high: close + 1.0,
            low: close - 2.0,
            close,
            volume: 10.0,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trader-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir); // stale leftovers from an earlier run, if any
        dir
    }

    #[test]
    fn round_trips_bars() {
        let store = Store::new(temp_dir("roundtrip"));
        let series = Series::new("TEST", vec![bar(0, 100.0), bar(1, 101.5)]);
        store.save(&series, "1h").expect("save");

        let loaded = store.load("TEST", "1h").expect("load").expect("exists");
        assert_eq!(loaded, series);
    }

    #[test]
    fn missing_file_is_none() {
        let store = Store::new(temp_dir("missing"));
        assert!(store.load("NOPE", "1h").expect("load").is_none());
    }

    #[test]
    fn rejects_unsorted_data() {
        let dir = temp_dir("unsorted");
        let path = dir.join("X-1h.parquet");
        write_parquet(&path, &[bar(1, 1.0), bar(0, 1.0)]).expect("write");
        assert!(matches!(
            read_parquet(&path),
            Err(DataError::Corrupt { .. })
        ));
    }
}

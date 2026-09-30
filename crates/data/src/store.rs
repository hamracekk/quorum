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
use trader_core::{Bar, BarStore, InstrumentId, InstrumentKind, PortError, Series, VenueId};

use crate::{DataError, Result};

/// `BarStore` on Parquet files: `<root>/<venue>/<SYMBOL>-<interval>[-<kind>].parquet`
/// (the kind suffix is omitted for spot).
#[derive(Debug, Clone)]
pub struct ParquetStore {
    root: PathBuf,
}

impl ParquetStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, instrument: &InstrumentId, interval: &str) -> Result<PathBuf> {
        if interval.is_empty() || !interval.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(DataError::InvalidInterval(interval.to_owned()));
        }
        let kind = match instrument.kind() {
            InstrumentKind::Spot => String::new(),
            other => format!("-{}", other.code()),
        };
        Ok(self
            .root
            .join(instrument.venue().as_str())
            .join(format!("{}-{interval}{kind}.parquet", instrument.symbol())))
    }

    /// Move files from the M1 flat layout (`<root>/<SYMBOL>-<interval>.parquet`) into
    /// `<root>/<venue>/`. Returns how many files were moved; existing targets are kept.
    pub fn migrate_flat_layout(&self, venue: &VenueId) -> Result<usize> {
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| DataError::Io { path, source }
        };
        if !self.root.is_dir() {
            return Ok(0);
        }
        let target_dir = self.root.join(venue.as_str());
        let mut moved = 0;
        for entry in fs::read_dir(&self.root).map_err(io(&self.root))? {
            let path = entry.map_err(io(&self.root))?.path();
            let is_parquet = path.extension().is_some_and(|ext| ext == "parquet");
            let Some(name) = path.file_name().filter(|_| path.is_file() && is_parquet) else {
                continue;
            };
            let target = target_dir.join(name);
            if target.exists() {
                tracing::warn!(from = %path.display(), to = %target.display(), "target exists; left in place");
                continue;
            }
            fs::create_dir_all(&target_dir).map_err(io(&target_dir))?;
            fs::rename(&path, &target).map_err(io(&path))?;
            moved += 1;
        }
        if moved > 0 {
            tracing::info!(moved, venue = %venue, "migrated bar files to per-venue layout");
        }
        Ok(moved)
    }
}

fn port(err: DataError) -> PortError {
    PortError::adapter("parquet store", err)
}

impl BarStore for ParquetStore {
    fn load(
        &self,
        instrument: &InstrumentId,
        interval: &str,
    ) -> std::result::Result<Option<Series>, PortError> {
        let path = self.path(instrument, interval).map_err(port)?;
        if !path.exists() {
            return Ok(None);
        }
        let bars = read_parquet(&path).map_err(port)?;
        Ok(Some(Series::new(instrument.clone(), bars)))
    }

    fn save(&self, series: &Series, interval: &str) -> std::result::Result<(), PortError> {
        let path = self.path(&series.instrument, interval).map_err(port)?;
        write_parquet(&path, &series.bars).map_err(port)
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

/// Read bars written by [`write_parquet`], verifying they are sorted and unique.
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

    fn btc() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
    }

    #[test]
    fn round_trips_bars_through_the_port() {
        let store = ParquetStore::new(temp_dir("roundtrip"));
        let series = Series::new(btc(), vec![bar(0, 100.0), bar(1, 101.5)]);
        store.save(&series, "1h").expect("save");

        let loaded = store.load(&btc(), "1h").expect("load").expect("exists");
        assert_eq!(loaded, series);
    }

    #[test]
    fn files_are_grouped_by_venue_with_kind_suffix() {
        let store = ParquetStore::new("/data");
        let spot = store.path(&btc(), "1h").expect("path");
        assert_eq!(spot, PathBuf::from("/data/binance/BTCUSDT-1h.parquet"));

        let perp: InstrumentId = "hyperliquid:BTC:perp".parse().expect("valid instrument");
        let perp = store.path(&perp, "4h").expect("path");
        assert_eq!(perp, PathBuf::from("/data/hyperliquid/BTC-4h-perp.parquet"));
    }

    #[test]
    fn rejects_path_like_intervals() {
        let store = ParquetStore::new("/data");
        assert!(matches!(
            store.path(&btc(), "../1h"),
            Err(DataError::InvalidInterval(_))
        ));
    }

    #[test]
    fn missing_file_is_none() {
        let store = ParquetStore::new(temp_dir("missing"));
        assert!(store.load(&btc(), "1h").expect("load").is_none());
    }

    #[test]
    fn migrates_flat_files_into_venue_directory() {
        let dir = temp_dir("migrate");
        write_parquet(&dir.join("BTCUSDT-1h.parquet"), &[bar(0, 1.0)]).expect("write");
        let store = ParquetStore::new(&dir);
        let venue = VenueId::new("binance").expect("valid venue");

        assert_eq!(store.migrate_flat_layout(&venue).expect("migrate"), 1);
        assert!(!dir.join("BTCUSDT-1h.parquet").exists());
        assert!(store.load(&btc(), "1h").expect("load").is_some());
        assert_eq!(store.migrate_flat_layout(&venue).expect("idempotent"), 0);
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

    #[test]
    fn perpetuals_round_trip_separately_from_spot() {
        let store = ParquetStore::new(temp_dir("perp"));
        let perp: InstrumentId = "binance:BTCUSDT:perp".parse().expect("valid instrument");
        store
            .save(&Series::new(perp.clone(), vec![bar(0, 5.0)]), "1h")
            .expect("save");
        assert!(store.load(&perp, "1h").expect("load").is_some());
        assert!(store.load(&btc(), "1h").expect("load").is_none());
    }

    #[test]
    fn missing_columns_are_reported_as_corrupt() {
        let dir = temp_dir("columns");
        fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("bad.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        )]));
        let ts = TimestampMillisecondArray::from(vec![0_i64]).with_timezone("UTC");
        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(ts)]).expect("batch");
        let mut writer = ArrowWriter::try_new(File::create(&path).expect("create"), schema, None)
            .expect("writer");
        writer.write(&batch).expect("write");
        writer.close().expect("close");

        assert!(matches!(
            read_parquet(&path),
            Err(DataError::Corrupt { .. })
        ));
    }

    #[test]
    fn invalid_interval_is_refused_through_the_port() {
        let store = ParquetStore::new(temp_dir("port-interval"));
        let err = store
            .save(&Series::new(btc(), vec![bar(0, 1.0)]), "1 h")
            .expect_err("invalid interval");
        assert!(matches!(err, PortError::Adapter { .. }));
    }
}

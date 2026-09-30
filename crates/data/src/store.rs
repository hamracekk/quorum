use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use trader_core::Bar;

pub fn write_csv(path: &Path, bars: &[Bar]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut writer = csv::Writer::from_path(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    for bar in bars {
        writer.serialize(bar)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn read_csv(path: &Path) -> Result<Vec<Bar>> {
    let mut reader =
        csv::Reader::from_path(path).with_context(|| format!("cannot open {}", path.display()))?;
    let bars = reader.deserialize().collect::<Result<Vec<Bar>, _>>()?;
    ensure!(
        bars.windows(2).all(|w| w[0].ts < w[1].ts),
        "{} is not sorted by time or has duplicate bars",
        path.display()
    );
    Ok(bars)
}

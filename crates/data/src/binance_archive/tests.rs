use std::io::Write as _;

use super::*;

#[test]
fn listing_tags_are_read_in_order() {
    let xml = "<a><Prefix>x/</Prefix><CommonPrefixes><Prefix>x/AUSDT/</Prefix></CommonPrefixes>\
               <CommonPrefixes><Prefix>x/BUSDT/</Prefix></CommonPrefixes><IsTruncated>false</IsTruncated></a>";
    assert_eq!(tags(xml, "Prefix"), ["x/", "x/AUSDT/", "x/BUSDT/"]);
    assert_eq!(tags(xml, "IsTruncated"), ["false"]);
}

#[test]
fn archive_rows_in_milliseconds_or_microseconds_become_bars() {
    let csv = "open_time,open,high,low,close,volume,close_time\n\
               1650000000000,1.0,2.0,0.5,1.5,100,1650086399999\n\
               1735689600000000,3.0,4.0,2.5,3.5,200,1735775999999999\n";
    let bars = parse_csv(csv).expect("bars");
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].ts.timestamp_millis(), 1_650_000_000_000);
    assert_eq!(
        bars[1].ts.timestamp_millis(),
        1_735_689_600_000,
        "microseconds scaled"
    );
    assert!((bars[1].close - 3.5).abs() < f64::EPSILON);
}

#[test]
fn a_zip_holding_one_csv_is_read() {
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        writer
            .start_file(
                "LUNAUSDT-1d-2022-05.csv",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("file");
        writer
            .write_all(b"1651363200000,80.0,82.0,60.0,65.0,1000,1651449599999\n")
            .expect("write");
        writer.finish().expect("finish");
    }
    let bars = parse_zip(buffer.get_ref()).expect("bars");
    assert_eq!(bars.len(), 1);
    assert!((bars[0].low - 60.0).abs() < f64::EPSILON);
}

#[test]
fn metrics_keep_the_days_last_reading() {
    let csv = "create_time,symbol,sum_open_interest,sum_open_interest_value,count_toptrader_long_short_ratio,sum_toptrader_long_short_ratio,count_long_short_ratio,sum_taker_long_short_vol_ratio\n\
               2024-03-01 00:00:00,BTCUSDT,71955.3,4403912741.4,1.26,1.53,1.15,0.76\n\
               2024-03-01 23:55:00,BTCUSDT,72187.7,4500000000.0,1.26,1.53,1.30,1.13\n";
    let day = chrono::NaiveDate::from_ymd_opt(2024, 3, 1).expect("date");
    let m = parse_metrics(csv, day).expect("parsed");
    assert!((m.open_interest_value - 4.5e9).abs() < 1.0);
    assert!((m.long_short_ratio - 1.30).abs() < 1e-12);
    assert_eq!(m.ts.date_naive(), day);
    assert!(parse_metrics("create_time,symbol\n", day).is_none());
}

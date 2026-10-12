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

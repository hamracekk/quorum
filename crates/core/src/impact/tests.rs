use super::*;

fn link(instrument: &str, sign: i8) -> ImpactLink {
    ImpactLink {
        instrument: instrument.parse().expect("instrument"),
        sign,
        why: "a mechanism".into(),
        since: NaiveDate::from_ymd_opt(2026, 10, 14).expect("date"),
        horizon: Horizon::Days,
    }
}

fn gpu() -> ImpactMap {
    ImpactMap {
        themes: vec![ImpactTheme {
            id: "gpu-prices".into(),
            description: "GPU and AI accelerator prices or shortages".into(),
            links: vec![
                link("tiingo:NVDA:equity", 1),
                link("tiingo:MARA:equity", -1),
            ],
        }],
    }
}

#[test]
fn a_theme_moving_implies_opposite_moves_through_its_signs() {
    let map = gpu();
    map.validate().expect("valid");
    let up = map.implications("gpu-prices", ThemeMove::Up);
    assert_eq!(up.len(), 2);
    assert_eq!(up[0].direction, Direction::Bullish, "the GPU maker gains");
    assert_eq!(up[1].direction, Direction::Bearish, "miners' costs rise");
    let down = map.implications("gpu-prices", ThemeMove::Down);
    assert_eq!(down[0].direction, Direction::Bearish);
    assert_eq!(down[1].direction, Direction::Bullish);
    assert!(map.implications("unknown", ThemeMove::Up).is_empty());
}

#[test]
fn bad_maps_are_rejected_with_the_reason() {
    let mut map = gpu();
    map.themes[0].links[0].sign = 2;
    assert!(map.validate().expect_err("sign").contains("sign"));
    let mut map = gpu();
    map.themes[0].id = "GPU prices".into();
    assert!(map.validate().is_err());
    let mut map = gpu();
    map.themes.push(map.themes[0].clone());
    assert!(map.validate().expect_err("dup").contains("twice"));
    let mut map = gpu();
    map.themes[0].links.push(link("tiingo:NVDA:equity", -1));
    assert!(map.validate().expect_err("dup link").contains("twice"));
    let mut map = gpu();
    map.themes[0].links[0].why = "line\nbreak".into();
    assert!(map.validate().is_err(), "control characters");
    let mut map = gpu();
    map.themes[0].links.clear();
    assert!(
        map.validate().is_err(),
        "a theme without links moves nothing"
    );
    assert!(ImpactMap::default().validate().is_ok() && ImpactMap::default().is_empty());
}

#[test]
fn the_map_reads_from_toml_like_config() {
    let map: ImpactMap = serde_json::from_value(serde_json::json!({
        "themes": [{
            "id": "spot-etf",
            "description": "Spot crypto ETF approvals, launches or inflows",
            "links": [{"instrument": "binance:BTCUSDT", "sign": 1, "why": "new demand",
                       "since": "2026-10-14"}]
        }]
    }))
    .expect("parses");
    assert_eq!(
        map.themes[0].links[0].horizon,
        Horizon::Days,
        "default horizon"
    );
    assert!(
        map.classifier_text()
            .starts_with("spot-etf: Spot crypto ETF")
    );
}

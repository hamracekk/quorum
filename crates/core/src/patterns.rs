//! Classic candlestick patterns.
//!
//! `detect_at(bars, i)` reports the patterns *completed* by bar `i`, looking only at
//! `bars[..=i]` — never ahead. Reversal patterns require the textbook prior trend (a 5-bar
//! decline for bullish reversals, a rise for bearish ones). Crypto trades 24/7 without gaps, so
//! rules that classically need a gap (stars, piercing line, dark cloud) use body relations
//! instead.

use serde::Serialize;

use crate::types::Bar;

/// Direction a signal suggests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Bias {
    Bullish,
    Bearish,
    Neutral,
}

impl Bias {
    /// +1 for bullish, −1 for bearish, 0 for neutral.
    pub const fn sign(self) -> f64 {
        match self {
            Self::Bullish => 1.0,
            Self::Bearish => -1.0,
            Self::Neutral => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pattern {
    Doji,
    Hammer,
    ShootingStar,
    BullishEngulfing,
    BearishEngulfing,
    BullishHarami,
    BearishHarami,
    PiercingLine,
    DarkCloudCover,
    MorningStar,
    EveningStar,
    ThreeWhiteSoldiers,
    ThreeBlackCrows,
}

impl Pattern {
    pub const ALL: [Self; 13] = [
        Self::Doji,
        Self::Hammer,
        Self::ShootingStar,
        Self::BullishEngulfing,
        Self::BearishEngulfing,
        Self::BullishHarami,
        Self::BearishHarami,
        Self::PiercingLine,
        Self::DarkCloudCover,
        Self::MorningStar,
        Self::EveningStar,
        Self::ThreeWhiteSoldiers,
        Self::ThreeBlackCrows,
    ];

    pub const fn bias(self) -> Bias {
        match self {
            Self::Doji => Bias::Neutral,
            Self::Hammer
            | Self::BullishEngulfing
            | Self::BullishHarami
            | Self::PiercingLine
            | Self::MorningStar
            | Self::ThreeWhiteSoldiers => Bias::Bullish,
            Self::ShootingStar
            | Self::BearishEngulfing
            | Self::BearishHarami
            | Self::DarkCloudCover
            | Self::EveningStar
            | Self::ThreeBlackCrows => Bias::Bearish,
        }
    }

    /// `snake_case` name, as serialized.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Doji => "doji",
            Self::Hammer => "hammer",
            Self::ShootingStar => "shooting_star",
            Self::BullishEngulfing => "bullish_engulfing",
            Self::BearishEngulfing => "bearish_engulfing",
            Self::BullishHarami => "bullish_harami",
            Self::BearishHarami => "bearish_harami",
            Self::PiercingLine => "piercing_line",
            Self::DarkCloudCover => "dark_cloud_cover",
            Self::MorningStar => "morning_star",
            Self::EveningStar => "evening_star",
            Self::ThreeWhiteSoldiers => "three_white_soldiers",
            Self::ThreeBlackCrows => "three_black_crows",
        }
    }
}

/// Bars of prior trend required before a reversal pattern.
const TREND_BARS: usize = 5;

struct Candle {
    open: f64,
    close: f64,
    high: f64,
    low: f64,
}

impl Candle {
    const fn of(bar: &Bar) -> Self {
        Self {
            open: bar.open,
            close: bar.close,
            high: bar.high,
            low: bar.low,
        }
    }
    fn body(&self) -> f64 {
        (self.close - self.open).abs()
    }
    fn range(&self) -> f64 {
        self.high - self.low
    }
    fn upper(&self) -> f64 {
        self.high - self.open.max(self.close)
    }
    fn lower(&self) -> f64 {
        self.open.min(self.close) - self.low
    }
    fn bullish(&self) -> bool {
        self.close > self.open
    }
    fn bearish(&self) -> bool {
        self.close < self.open
    }
    const fn body_mid(&self) -> f64 {
        f64::midpoint(self.open, self.close)
    }
    /// Body at least half the range.
    fn strong(&self) -> bool {
        self.range() > 0.0 && self.body() >= 0.5 * self.range()
    }
}

/// Closes fell over the `TREND_BARS` bars ending at `end` (inclusive).
fn falling(bars: &[Bar], end: usize) -> bool {
    end >= TREND_BARS && bars[end].close < bars[end - TREND_BARS].close
}

fn rising(bars: &[Bar], end: usize) -> bool {
    end >= TREND_BARS && bars[end].close > bars[end - TREND_BARS].close
}

/// Patterns completed by `bars[i]`, using only `bars[..=i]`.
pub fn detect_at(bars: &[Bar], i: usize) -> Vec<Pattern> {
    let mut found = Vec::new();
    let Some(bar) = bars.get(i) else {
        return found;
    };
    let c = Candle::of(bar);
    if c.range() > 0.0 && c.body() <= 0.1 * c.range() {
        found.push(Pattern::Doji);
    }
    if i >= 1 {
        one_bar_reversals(bars, i, &c, &mut found);
        two_bar(bars, i, &c, &mut found);
    }
    if i >= 2 {
        three_bar(bars, i, &c, &mut found);
    }
    found
}

/// Hammer and shooting star: a long wick against the prior trend.
fn one_bar_reversals(bars: &[Bar], i: usize, c: &Candle, found: &mut Vec<Pattern>) {
    if c.body() <= 0.0 {
        return;
    }
    if falling(bars, i - 1) && c.lower() >= 2.0 * c.body() && c.upper() <= 0.5 * c.body() {
        found.push(Pattern::Hammer);
    }
    if rising(bars, i - 1) && c.upper() >= 2.0 * c.body() && c.lower() <= 0.5 * c.body() {
        found.push(Pattern::ShootingStar);
    }
}

/// Engulfing, harami, piercing line and dark cloud cover.
fn two_bar(bars: &[Bar], i: usize, c: &Candle, found: &mut Vec<Pattern>) {
    let p = Candle::of(&bars[i - 1]);
    let down = falling(bars, i - 1);
    let up = rising(bars, i - 1);
    let bull_turn = down && p.bearish() && c.bullish();
    let bear_turn = up && p.bullish() && c.bearish();
    if bull_turn && c.open <= p.close && c.close >= p.open && c.body() > p.body() {
        found.push(Pattern::BullishEngulfing);
    }
    if bear_turn && c.open >= p.close && c.close <= p.open && c.body() > p.body() {
        found.push(Pattern::BearishEngulfing);
    }
    if bull_turn && p.strong() && c.open > p.close && c.close < p.open {
        found.push(Pattern::BullishHarami);
    }
    if bear_turn && p.strong() && c.open < p.close && c.close > p.open {
        found.push(Pattern::BearishHarami);
    }
    if bull_turn && p.strong() && c.open <= p.close && c.close > p.body_mid() && c.close < p.open {
        found.push(Pattern::PiercingLine);
    }
    if bear_turn && p.strong() && c.open >= p.close && c.close < p.body_mid() && c.close > p.open {
        found.push(Pattern::DarkCloudCover);
    }
}

/// Morning/evening star and three white soldiers / black crows.
fn three_bar(bars: &[Bar], i: usize, c: &Candle, found: &mut Vec<Pattern>) {
    let (a, b) = (Candle::of(&bars[i - 2]), Candle::of(&bars[i - 1]));
    let small_middle = b.body() <= 0.3 * a.body();
    let first_strong_bear = a.bearish() && a.strong();
    let first_strong_bull = a.bullish() && a.strong();
    if falling(bars, i - 2)
        && first_strong_bear
        && small_middle
        && c.bullish()
        && c.close > a.body_mid()
    {
        found.push(Pattern::MorningStar);
    }
    if rising(bars, i - 2)
        && first_strong_bull
        && small_middle
        && c.bearish()
        && c.close < a.body_mid()
    {
        found.push(Pattern::EveningStar);
    }
    let three = [&a, &b, c];
    let soldiers = three
        .iter()
        .all(|x| x.bullish() && x.strong() && x.upper() <= 0.3 * x.body())
        && b.close > a.close
        && c.close > b.close
        && b.open > a.open
        && b.open < a.close
        && c.open > b.open
        && c.open < b.close;
    if soldiers {
        found.push(Pattern::ThreeWhiteSoldiers);
    }
    let crows = three
        .iter()
        .all(|x| x.bearish() && x.strong() && x.lower() <= 0.3 * x.body())
        && b.close < a.close
        && c.close < b.close
        && b.open < a.open
        && b.open > a.close
        && c.open < b.open
        && c.open > b.close;
    if crows {
        found.push(Pattern::ThreeBlackCrows);
    }
}

/// Every pattern occurrence: `(index of completing bar, pattern)`.
pub fn detect(bars: &[Bar]) -> Vec<(usize, Pattern)> {
    (0..bars.len())
        .flat_map(|i| detect_at(bars, i).into_iter().map(move |p| (i, p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    fn candle(i: i64, open: f64, high: f64, low: f64, close: f64) -> Bar {
        Bar {
            ts: DateTime::from_timestamp(i * 3_600, 0).expect("valid timestamp"),
            open,
            high,
            low,
            close,
            volume: 1.0,
        }
    }

    /// Six bars falling from 110 to 100 (closes), establishing a downtrend.
    fn downtrend() -> Vec<Bar> {
        (0..6)
            .map(|i| {
                let close = 2.0_f64.mul_add(-(i as f64), 110.0);
                candle(i, close + 1.0, close + 1.5, close - 0.5, close)
            })
            .collect()
    }

    fn uptrend() -> Vec<Bar> {
        (0..6)
            .map(|i| {
                let close = 2.0_f64.mul_add(i as f64, 100.0);
                candle(i, close - 1.0, close + 0.5, close - 1.5, close)
            })
            .collect()
    }

    fn found_last(bars: &[Bar]) -> Vec<Pattern> {
        detect_at(bars, bars.len() - 1)
    }

    #[test]
    fn doji_has_a_tiny_body() {
        assert!(found_last(&[candle(0, 100.0, 105.0, 95.0, 100.5)]).contains(&Pattern::Doji));
        assert!(!found_last(&[candle(0, 100.0, 105.0, 95.0, 104.0)]).contains(&Pattern::Doji));
    }

    #[test]
    fn hammer_needs_a_long_lower_wick_after_a_decline() {
        let mut bars = downtrend();
        bars.push(candle(6, 99.0, 99.6, 94.0, 99.5));
        assert!(found_last(&bars).contains(&Pattern::Hammer));

        let mut flat_trend = uptrend();
        flat_trend.push(candle(6, 99.0, 99.6, 94.0, 99.5));
        assert!(
            !found_last(&flat_trend).contains(&Pattern::Hammer),
            "context matters"
        );
    }

    #[test]
    fn shooting_star_needs_a_long_upper_wick_after_a_rise() {
        let mut bars = uptrend();
        bars.push(candle(6, 111.0, 116.0, 110.8, 110.5));
        assert!(found_last(&bars).contains(&Pattern::ShootingStar));
    }

    #[test]
    fn engulfing_bodies_cover_the_previous_body() {
        let mut bull = downtrend();
        bull.push(candle(6, 101.0, 101.2, 99.0, 99.2));
        bull.push(candle(7, 99.0, 102.5, 98.8, 102.0));
        assert!(found_last(&bull).contains(&Pattern::BullishEngulfing));

        let mut bear = uptrend();
        bear.push(candle(6, 109.0, 111.2, 108.8, 111.0));
        bear.push(candle(7, 111.2, 111.4, 108.0, 108.5));
        assert!(found_last(&bear).contains(&Pattern::BearishEngulfing));
    }

    #[test]
    fn harami_sits_inside_a_strong_previous_body() {
        let mut bars = downtrend();
        bars.push(candle(6, 104.0, 104.2, 97.8, 98.0));
        bars.push(candle(7, 99.0, 101.5, 98.8, 101.0));
        assert!(found_last(&bars).contains(&Pattern::BullishHarami));
    }

    #[test]
    fn piercing_line_closes_above_the_midpoint() {
        let mut bars = downtrend();
        bars.push(candle(6, 104.0, 104.2, 97.8, 98.0));
        bars.push(candle(7, 98.0, 102.5, 97.5, 102.0));
        assert!(found_last(&bars).contains(&Pattern::PiercingLine));
    }

    #[test]
    fn dark_cloud_closes_below_the_midpoint() {
        let mut bars = uptrend();
        bars.push(candle(6, 106.0, 112.2, 105.8, 112.0));
        bars.push(candle(7, 112.0, 112.5, 107.5, 108.0));
        assert!(found_last(&bars).contains(&Pattern::DarkCloudCover));
    }

    #[test]
    fn morning_and_evening_stars_have_a_small_middle_candle() {
        let mut morning = downtrend();
        morning.push(candle(6, 104.0, 104.2, 97.8, 98.0));
        morning.push(candle(7, 98.0, 98.6, 97.2, 97.8));
        morning.push(candle(8, 98.0, 103.0, 97.9, 102.5));
        assert!(found_last(&morning).contains(&Pattern::MorningStar));

        let mut evening = uptrend();
        evening.push(candle(6, 106.0, 112.2, 105.8, 112.0));
        evening.push(candle(7, 112.0, 112.8, 111.4, 112.2));
        evening.push(candle(8, 112.0, 112.1, 107.0, 107.5));
        assert!(found_last(&evening).contains(&Pattern::EveningStar));
    }

    #[test]
    fn three_soldiers_and_crows_step_steadily() {
        let soldiers = [
            candle(0, 100.0, 104.2, 99.8, 104.0),
            candle(1, 102.0, 106.2, 101.8, 106.0),
            candle(2, 104.0, 108.2, 103.8, 108.0),
        ];
        assert!(found_last(&soldiers).contains(&Pattern::ThreeWhiteSoldiers));
        let crows = [
            candle(0, 108.0, 108.2, 103.8, 104.0),
            candle(1, 106.0, 106.2, 101.8, 102.0),
            candle(2, 104.0, 104.2, 99.8, 100.0),
        ];
        assert!(found_last(&crows).contains(&Pattern::ThreeBlackCrows));
    }

    #[test]
    fn detection_never_looks_ahead() {
        let mut bars = downtrend();
        bars.push(candle(6, 101.0, 101.2, 99.0, 99.2));
        bars.push(candle(7, 99.0, 102.5, 98.8, 102.0));
        bars.push(candle(8, 150.0, 160.0, 40.0, 60.0));
        for i in 0..bars.len() {
            assert_eq!(detect_at(&bars, i), detect_at(&bars[..=i], i), "bar {i}");
        }
    }

    #[test]
    fn every_pattern_has_a_bias_and_a_unique_name() {
        let mut names: Vec<&str> = Pattern::ALL.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Pattern::ALL.len());
        assert_eq!(Pattern::Doji.bias(), Bias::Neutral);
        assert!((Pattern::MorningStar.bias().sign() - 1.0).abs() < f64::EPSILON);
    }
}

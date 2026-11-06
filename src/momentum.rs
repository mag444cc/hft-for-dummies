//! Which way is Binance going, right now?
//!
//! Binance moves first, and over a second or so its moves tend to carry on.
//! If it has just gone up, Hyperliquid is likely to follow, and a sell order
//! left at the old price is the one that gets picked off. So the market maker
//! can lean both quotes the way Binance just went (`momentum_lean`). This
//! measures that move: how far Binance's mid went over the last `lookback`,
//! in basis points, signed (up is positive).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;

/// Keep at most one price per this long.
const SAMPLE_EVERY: Duration = Duration::from_millis(20);

pub struct Momentum {
    /// (time, Binance mid), oldest first.
    prices: VecDeque<(Instant, Decimal)>,
    lookback: Duration,
}

impl Momentum {
    pub fn new(lookback: Duration) -> Self {
        Self {
            prices: VecDeque::new(),
            lookback,
        }
    }

    /// A new Binance mid.
    pub fn on_price(&mut self, mid: Decimal, at: Instant) {
        if self
            .prices
            .back()
            .is_some_and(|(last, _)| at.saturating_duration_since(*last) < SAMPLE_EVERY)
        {
            // Too soon for a new sample, but keep the latest price.
            if let Some(back) = self.prices.back_mut() {
                back.1 = mid;
            }
            return;
        }
        self.prices.push_back((at, mid));
        // Keep one price from at least `lookback` ago, and nothing older.
        while self.prices.len() > 1
            && self.prices[1].0 <= at.checked_sub(self.lookback).unwrap_or(at)
        {
            self.prices.pop_front();
        }
    }

    /// How far Binance moved over the last `lookback` (bps, up is positive),
    /// or `None` until we have that much history.
    pub fn move_bps(&self) -> Option<Decimal> {
        let (then_at, then) = *self.prices.front()?;
        let (now_at, now) = *self.prices.back()?;
        if now_at.saturating_duration_since(then_at) < self.lookback || then.is_zero() {
            return None;
        }
        Some(((now - then) / then * Decimal::from(10_000)).round_dp(4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn measures_the_move_over_the_lookback() {
        let mut m = Momentum::new(ms(1000));
        let start = Instant::now();
        // Flat for a while, then up 5 bps (100 -> 100.05) over the last second.
        for step in 0..100u64 {
            let price = if step < 50 {
                d("100")
            } else {
                d("100") + d("0.001") * Decimal::from(step - 50)
            };
            m.on_price(price, start + ms(step * 20));
        }
        // Last price 100.049, a second earlier 100.0: about +4.9 bps.
        let moved = m.move_bps().unwrap();
        assert!((moved - d("4.9")).abs() < d("0.11"), "{moved}");
    }

    #[test]
    fn down_is_negative() {
        let mut m = Momentum::new(ms(1000));
        let start = Instant::now();
        m.on_price(d("100"), start);
        m.on_price(d("99.97"), start + ms(1000));
        assert_eq!(m.move_bps(), Some(d("-3")));
    }

    #[test]
    fn needs_a_full_lookback_first() {
        let mut m = Momentum::new(ms(1000));
        let start = Instant::now();
        m.on_price(d("100"), start);
        m.on_price(d("100.1"), start + ms(500));
        assert_eq!(m.move_bps(), None);
    }

    #[test]
    fn only_the_recent_move_counts() {
        // A big jump long ago, flat since: no momentum now.
        let mut m = Momentum::new(ms(1000));
        let start = Instant::now();
        m.on_price(d("100"), start);
        m.on_price(d("101"), start + ms(100));
        for step in 1..=150u64 {
            m.on_price(d("101"), start + ms(100 + step * 20));
        }
        assert_eq!(m.move_bps(), Some(Decimal::ZERO));
    }
}

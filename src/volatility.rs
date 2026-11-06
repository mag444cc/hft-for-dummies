//! How jumpy is the market right now?
//!
//! A quote can only move every so often (`min_requote_ms`). If the price
//! moves further than our spread before we can move, someone trades with our
//! stale quote and we lose. So the spread should cover a typical move over
//! that time. This measures it from Binance's recent prices: the average size
//! of a move over `horizon`, across the last `window`, in basis points.
//!
//! Busy minutes give a big number (quote wider), calm ones a small one
//! (quote tighter). `strategy.rs` turns it into a spread.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;

/// Keep at most one price per this long: enough detail, little memory.
const SAMPLE_EVERY: Duration = Duration::from_millis(100);

pub struct Volatility {
    /// (time, Binance mid), oldest first, one per `SAMPLE_EVERY` at most.
    prices: VecDeque<(Instant, Decimal)>,
    /// How far apart the two prices of a move are.
    horizon: Duration,
    /// How much history to average over.
    window: Duration,
}

impl Volatility {
    pub fn new(horizon: Duration, window: Duration) -> Self {
        Self {
            prices: VecDeque::new(),
            horizon,
            window,
        }
    }

    /// A new Binance mid.
    pub fn on_price(&mut self, mid: Decimal, at: Instant) {
        if self
            .prices
            .back()
            .is_some_and(|(last, _)| at.saturating_duration_since(*last) < SAMPLE_EVERY)
        {
            return;
        }
        self.prices.push_back((at, mid));
        // Keep the window, plus one horizon so the oldest move is complete.
        while self
            .prices
            .front()
            .is_some_and(|(t, _)| at.saturating_duration_since(*t) > self.window + self.horizon)
        {
            self.prices.pop_front();
        }
    }

    /// The average move over `horizon`, in bps, or `None` until we have seen
    /// at least half a window of prices.
    pub fn move_bps(&self) -> Option<Decimal> {
        let (first, last) = (self.prices.front()?.0, self.prices.back()?.0);
        if last.saturating_duration_since(first) < self.window / 2 + self.horizon {
            return None;
        }
        // Pair each price with the first one at least `horizon` later.
        let (mut total, mut count) = (Decimal::ZERO, 0u32);
        let mut later = 0;
        for (i, (t, price)) in self.prices.iter().enumerate() {
            later = later.max(i + 1);
            while later < self.prices.len()
                && self.prices[later].0.saturating_duration_since(*t) < self.horizon
            {
                later += 1;
            }
            let Some((_, then)) = self.prices.get(later) else {
                break;
            };
            if price.is_zero() {
                continue;
            }
            total += ((*then - *price) / *price).abs();
            count += 1;
        }
        (count > 0).then(|| (total / Decimal::from(count) * Decimal::from(10_000)).round_dp(4))
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

    /// One price every 100 ms for `secs`, from `price(step)`.
    fn feed(v: &mut Volatility, start: Instant, secs: u64, price: impl Fn(u64) -> Decimal) {
        for step in 0..secs * 10 {
            v.on_price(price(step), start + ms(step * 100));
        }
    }

    #[test]
    fn a_still_market_has_no_moves() {
        let mut v = Volatility::new(ms(1000), Duration::from_secs(60));
        feed(&mut v, Instant::now(), 60, |_| d("100"));
        assert_eq!(v.move_bps(), Some(Decimal::ZERO));
    }

    #[test]
    fn measures_the_typical_move_over_the_horizon() {
        // Up 1 bp (0.01) every second, steadily: every 1 s move is 1 bp.
        let mut v = Volatility::new(ms(1000), Duration::from_secs(60));
        feed(&mut v, Instant::now(), 60, |step| {
            d("100") + d("0.001") * Decimal::from(step)
        });
        let moved = v.move_bps().unwrap();
        assert!((moved - Decimal::ONE).abs() < d("0.01"), "{moved}");
    }

    #[test]
    fn a_longer_horizon_sees_bigger_moves() {
        let start = Instant::now();
        let (mut short, mut long) = (
            Volatility::new(ms(500), Duration::from_secs(60)),
            Volatility::new(ms(2000), Duration::from_secs(60)),
        );
        let price = |step: u64| d("100") + d("0.001") * Decimal::from(step);
        feed(&mut short, start, 60, price);
        feed(&mut long, start, 60, price);
        assert!(long.move_bps().unwrap() > short.move_bps().unwrap() * d("3"));
    }

    #[test]
    fn up_and_down_both_count() {
        // Swings 1 bp up and back down every second: moves are 1 bp either way.
        let mut v = Volatility::new(ms(1000), Duration::from_secs(60));
        feed(&mut v, Instant::now(), 60, |step| {
            if (step / 10) % 2 == 0 {
                d("100")
            } else {
                d("100.01")
            }
        });
        let moved = v.move_bps().unwrap();
        assert!((moved - Decimal::ONE).abs() < d("0.01"), "{moved}");
    }

    #[test]
    fn needs_some_history_first() {
        let mut v = Volatility::new(ms(1000), Duration::from_secs(60));
        feed(&mut v, Instant::now(), 20, |_| d("100"));
        assert_eq!(v.move_bps(), None, "20 s of a 60 s window");
    }

    #[test]
    fn forgets_old_prices() {
        // A jumpy first minute, then a still one: only the still one counts.
        let mut v = Volatility::new(ms(1000), Duration::from_secs(60));
        let start = Instant::now();
        feed(&mut v, start, 60, |step| {
            if step % 2 == 0 { d("100") } else { d("101") }
        });
        feed(&mut v, start + Duration::from_secs(60), 62, |_| d("100"));
        assert_eq!(v.move_bps(), Some(Decimal::ZERO));
    }
}

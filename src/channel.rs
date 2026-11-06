//! Is Binance moving in a clean channel, and where in it is the price?
//!
//! Draw a straight line through the last couple of hours of Binance prices
//! (a least-squares fit, like a trendline drawn by hand). If the price has
//! followed that line closely, it's a *channel*: prices tend to bounce
//! between its edges, so a price near the lower edge is a little more likely
//! to rise next, and near the upper edge to fall. On 17-52 days of
//! Hyperliquid candles that tilt was real but small (roughly 53-58%), so the
//! market maker only leans its quotes a little with it (`channel_lean_bps`).
//!
//! Far outside the channel the line has broken, and breaks tend to keep
//! going, so there is no lean then.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Samples per window: one a minute over two hours.
const SAMPLES: u32 = 120;

/// Beyond this many typical distances from the line, the channel has broken.
pub const BROKEN: f64 = 3.0;

pub struct Channel {
    /// (time, log price), oldest first, at most one per `step`.
    prices: VecDeque<(Instant, f64)>,
    window: Duration,
    step: Duration,
}

/// Where the latest price sits in a clean channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    /// How closely prices followed the line: 0 (not at all) to 1 (exactly).
    pub fit: f64,
    /// Distance from the line in typical distances: -1 is near the lower
    /// edge, +1 near the upper edge.
    pub distance: f64,
}

impl Channel {
    pub fn new(window: Duration) -> Self {
        Self {
            prices: VecDeque::new(),
            window,
            step: window / SAMPLES,
        }
    }

    /// A new Binance mid.
    pub fn on_price(&mut self, mid: f64, at: Instant) {
        if mid <= 0.0 {
            return;
        }
        let price = mid.ln();
        match self.prices.back_mut() {
            // Keep the latest price, but only one sample per step.
            Some((last, p)) if at.saturating_duration_since(*last) < self.step => *p = price,
            _ => self.prices.push_back((at, price)),
        }
        while self
            .prices
            .front()
            .is_some_and(|(t, _)| at.saturating_duration_since(*t) > self.window)
        {
            self.prices.pop_front();
        }
    }

    /// The fit and where the price is, once we have most of a window of history.
    pub fn position(&self) -> Option<Position> {
        let (first, _) = *self.prices.front()?;
        let (last, _) = *self.prices.back()?;
        if last.saturating_duration_since(first) < self.window * 9 / 10 || self.prices.len() < 10 {
            return None;
        }
        let n = self.prices.len() as f64;
        let xs: Vec<f64> = self
            .prices
            .iter()
            .map(|(t, _)| t.saturating_duration_since(first).as_secs_f64())
            .collect();
        let ys: Vec<f64> = self.prices.iter().map(|(_, p)| *p).collect();
        let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
        let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
        let syy: f64 = ys.iter().map(|y| (y - my).powi(2)).sum();
        if sxx == 0.0 || syy == 0.0 {
            return None;
        }
        let slope = xs
            .iter()
            .zip(&ys)
            .map(|(x, y)| (x - mx) * (y - my))
            .sum::<f64>()
            / sxx;
        let off = |x: f64, y: f64| y - (my + slope * (x - mx));
        let sse: f64 = xs.iter().zip(&ys).map(|(x, y)| off(*x, *y).powi(2)).sum();
        let spread = (sse / n).sqrt();
        if spread == 0.0 {
            return None;
        }
        Some(Position {
            fit: 1.0 - sse / syy,
            distance: off(xs[xs.len() - 1], ys[ys.len() - 1]) / spread,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Two hours of a steady climb from 100 with a small wiggle, ending at `last`.
    fn climb(last: f64) -> Channel {
        let mut c = Channel::new(secs(7200));
        let start = Instant::now();
        for m in 0..120u64 {
            let wiggle = if m % 2 == 0 { 0.02 } else { -0.02 };
            c.on_price(100.0 + 0.01 * m as f64 + wiggle, start + secs(60 * m));
        }
        c.on_price(last, start + secs(7200));
        c
    }

    #[test]
    fn a_steady_climb_is_a_clean_channel() {
        let p = climb(101.2).position().unwrap();
        assert!(p.fit > 0.9, "{p:?}");
        assert!(p.distance.abs() < 1.0, "on the line: {p:?}");
    }

    #[test]
    fn a_dip_in_the_climb_sits_near_the_lower_edge() {
        let p = climb(101.17).position().unwrap();
        assert!(p.distance < -1.0, "{p:?}");
    }

    #[test]
    fn a_crash_breaks_the_channel() {
        let p = climb(100.5).position().unwrap();
        assert!(p.distance < -BROKEN, "{p:?}");
    }

    #[test]
    fn nothing_until_most_of_the_window_is_seen() {
        let mut c = Channel::new(secs(7200));
        let start = Instant::now();
        for m in 0..60u64 {
            c.on_price(100.0 + 0.01 * m as f64, start + secs(60 * m));
        }
        assert_eq!(c.position(), None);
    }

    #[test]
    fn a_flat_choppy_market_is_not_a_clean_channel() {
        let mut c = Channel::new(secs(7200));
        let start = Instant::now();
        for m in 0..=120u64 {
            let p = if m % 3 == 0 { 100.1 } else { 99.95 };
            c.on_price(p, start + secs(60 * m));
        }
        assert!(c.position().unwrap().fit < 0.2);
    }
}

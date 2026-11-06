//! Chart indicators on Binance's price: RSI and MACD on candles the bot
//! builds itself.
//!
//! These are the classic chart-reading tools. On 3.5 days to 7 months of
//! Hyperliquid candles a few of their signals held on SOL, ETH and BTC
//! alike: a 5-minute RSI above 70 tended to fade over the next half hour,
//! a 1-hour RSI below 30 tended to recover over the next hours, and a
//! 1-minute MACD turning down from a high was slightly more often followed
//! by a fall. The edges are small, so the market maker only leans its
//! quotes a little with them (`rsi_fast_lean_bps`, `rsi_slow_lean_bps`,
//! `macd_lean_bps`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Candles closed every `every`, from the prices seen in between.
pub struct Candles {
    every: Duration,
    /// When the current candle started, and its latest price.
    open: Option<(Instant, f64)>,
    /// Closing prices, oldest first.
    closes: VecDeque<f64>,
    keep: usize,
}

impl Candles {
    pub fn new(every: Duration, keep: usize) -> Self {
        Self {
            every,
            open: None,
            closes: VecDeque::new(),
            keep,
        }
    }

    /// A new price. Returns true when it closed a candle.
    pub fn on_price(&mut self, price: f64, at: Instant) -> bool {
        if price <= 0.0 {
            return false;
        }
        match self.open {
            None => {
                self.open = Some((at, price));
                false
            }
            Some((started, last)) if at.saturating_duration_since(started) >= self.every => {
                // Close the candle at its last price; this price opens the next.
                self.closes.push_back(last);
                if self.closes.len() > self.keep {
                    self.closes.pop_front();
                }
                self.open = Some((started + self.every, price));
                true
            }
            Some((started, _)) => {
                self.open = Some((started, price));
                false
            }
        }
    }

    pub fn closes(&self) -> &VecDeque<f64> {
        &self.closes
    }
}

/// RSI over `period` candles (Wilder's smoothing), 0 to 100; `None` until
/// there are enough closes.
pub fn rsi(closes: &VecDeque<f64>, period: usize) -> Option<f64> {
    if closes.len() <= period {
        return None;
    }
    let (mut gain, mut loss) = (0.0, 0.0);
    for (i, pair) in closes.iter().zip(closes.iter().skip(1)).enumerate() {
        let change = pair.1 - pair.0;
        let (up, down) = (change.max(0.0), (-change).max(0.0));
        if i < period {
            gain += up / period as f64;
            loss += down / period as f64;
        } else {
            gain = (gain * (period - 1) as f64 + up) / period as f64;
            loss = (loss * (period - 1) as f64 + down) / period as f64;
        }
    }
    Some(if loss == 0.0 {
        100.0
    } else {
        100.0 - 100.0 / (1.0 + gain / loss)
    })
}

/// MACD (12, 26, 9): for each close, the MACD line and its histogram.
pub fn macd(closes: &VecDeque<f64>) -> Vec<(f64, f64)> {
    let ema = |xs: &[f64], n: f64| {
        let k = 2.0 / (n + 1.0);
        let mut out = Vec::with_capacity(xs.len());
        for x in xs {
            let next = out.last().map_or(*x, |prev: &f64| prev + k * (x - prev));
            out.push(next);
        }
        out
    };
    let xs: Vec<f64> = closes.iter().copied().collect();
    let line: Vec<f64> = ema(&xs, 12.0)
        .iter()
        .zip(ema(&xs, 26.0))
        .map(|(a, b)| a - b)
        .collect();
    let signal = ema(&line, 9.0);
    line.iter().zip(signal).map(|(m, s)| (*m, m - s)).collect()
}

/// Did MACD just turn: +1 up from well below zero, -1 down from well above,
/// 0 otherwise. "Well" = more than `threshold` (a fraction of the price).
pub fn macd_turn(closes: &VecDeque<f64>, threshold: f64) -> i32 {
    if closes.len() < 40 {
        return 0;
    }
    let m = macd(closes);
    let price = closes[closes.len() - 1];
    let [(_, h2), (_, h1), (line, h0)] = [m[m.len() - 3], m[m.len() - 2], m[m.len() - 1]];
    if line < -threshold * price && h1 < h2 && h0 > h1 {
        1
    } else if line > threshold * price && h1 > h2 && h0 < h1 {
        -1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closes(xs: &[f64]) -> VecDeque<f64> {
        xs.iter().copied().collect()
    }

    #[test]
    fn candles_close_on_time_at_their_last_price() {
        let mut c = Candles::new(Duration::from_secs(60), 10);
        let t = Instant::now();
        assert!(!c.on_price(100.0, t));
        assert!(!c.on_price(101.0, t + Duration::from_secs(30)));
        assert!(
            c.on_price(102.0, t + Duration::from_secs(61)),
            "a minute in"
        );
        assert_eq!(c.closes(), &closes(&[101.0]));
        assert!(c.on_price(103.0, t + Duration::from_secs(125)));
        assert_eq!(c.closes(), &closes(&[101.0, 102.0]));
    }

    #[test]
    fn rsi_is_high_after_gains_and_low_after_losses() {
        // Up 1, up 1, up 1, down 0.5, ...: mostly gains.
        let mut up = vec![100.0];
        for i in 1..20 {
            let step = if i % 4 == 0 { -0.5 } else { 1.0 };
            up.push(up[i - 1] + step);
        }
        let down: Vec<f64> = up.iter().map(|p| 200.0 - p).collect();
        assert!(rsi(&closes(&up), 14).unwrap() > 70.0);
        assert!(rsi(&closes(&down), 14).unwrap() < 30.0);
        assert_eq!(rsi(&closes(&up[..10]), 14), None, "not enough candles");
    }

    #[test]
    fn macd_turns_down_after_a_climb_runs_out() {
        // A speeding-up climb, then a dip: the histogram peaks and turns down.
        let mut xs: Vec<f64> = (0..50).map(|i| 100.0 + 0.005 * (i * i) as f64).collect();
        let mut turned = 0;
        for _ in 0..10 {
            xs.push(xs[xs.len() - 1] - 0.3);
            let t = macd_turn(&closes(&xs), 0.0003);
            if t != 0 {
                turned = t;
                break;
            }
        }
        assert_eq!(turned, -1);
    }

    #[test]
    fn macd_turns_up_after_a_fall_runs_out() {
        let mut xs: Vec<f64> = (0..50).map(|i| 100.0 - 0.005 * (i * i) as f64).collect();
        let mut turned = 0;
        for _ in 0..10 {
            xs.push(xs[xs.len() - 1] + 0.3);
            let t = macd_turn(&closes(&xs), 0.0003);
            if t != 0 {
                turned = t;
                break;
            }
        }
        assert_eq!(turned, 1);
    }

    #[test]
    fn no_macd_turn_in_a_flat_market() {
        let xs: Vec<f64> = (0..60)
            .map(|i| 100.0 + if i % 2 == 0 { 0.01 } else { -0.01 })
            .collect();
        assert_eq!(macd_turn(&closes(&xs), 0.0003), 0);
    }
}

//! Fair value: what we think the coin is worth right now.
//!
//! Binance moves first, so we start from the Binance mid. But Binance quotes
//! in USDT and the exchange we trade on in USD, and the two venues rarely
//! trade at exactly the same price. That steady gap is the *basis*. We
//! average it slowly, then:
//!
//! ```text
//! fair value = Binance mid + average basis
//! ```
//!
//! This follows Binance tick by tick without drifting away from where our
//! exchange actually trades.
//!
//! A second reference (Bybit) can be blended in: it gets its own basis, so
//! it gives its own estimate, and fair value is a weighted average of the
//! two. If one goes quiet, the other carries on alone; because each has its
//! own basis, the switch doesn't make fair value jump.
//!
//! If we don't have a trustworthy fair value (every reference went quiet, or
//! we haven't watched the basis long enough) we say so, and the bot stops
//! quoting. Quoting on a bad price is how market makers lose money.

use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

/// Watch the basis this long before trusting it.
pub const WARMUP: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct FairValue {
    binance: Reference,
    bybit: Reference,
    /// Bybit's share of the blend when both are fresh (0 = ignore it unless
    /// Binance has gone quiet).
    bybit_weight: Decimal,
}

/// One reference exchange: its latest mid and its basis to our exchange.
#[derive(Debug, Default)]
struct Reference {
    /// Latest mid and when it arrived.
    mid: Option<(Decimal, Instant)>,
    basis: Option<Basis>,
}

#[derive(Debug, Clone, Copy)]
struct Basis {
    /// Running average of (our exchange's mid − this reference's mid).
    average: Decimal,
    last_update: Instant,
    started: Instant,
}

/// Why there is no fair value right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    /// No reference price has arrived yet.
    NoPrice,
    /// Every reference has gone quiet.
    Stale,
    WarmingUp,
}

impl FairValue {
    /// `bybit_weight`: Bybit's share of the blend, from 0 to 1.
    pub fn new(bybit_weight: Decimal) -> Self {
        Self {
            bybit_weight,
            ..Self::default()
        }
    }

    /// A new best bid/ask from Binance.
    pub fn on_binance(&mut self, bid: Decimal, ask: Decimal, at: Instant) {
        self.binance.on_price(bid, ask, at);
    }

    /// A new best bid/ask from Bybit.
    pub fn on_bybit(&mut self, bid: Decimal, ask: Decimal, at: Instant) {
        self.bybit.on_price(bid, ask, at);
    }

    /// A new mid on our exchange: fold today's gap into each running average.
    pub fn on_venue_mid(
        &mut self,
        venue_mid: Decimal,
        now: Instant,
        stale: Duration,
        halflife: Duration,
    ) {
        self.binance.on_venue_mid(venue_mid, now, stale, halflife);
        self.bybit.on_venue_mid(venue_mid, now, stale, halflife);
    }

    /// The price to quote around, or why there isn't one.
    pub fn get(&self, now: Instant, stale: Duration) -> Result<Decimal, NotReady> {
        match (
            self.binance.estimate(now, stale),
            self.bybit.estimate(now, stale),
        ) {
            (Ok(binance), Ok(bybit)) => {
                Ok(binance * (Decimal::ONE - self.bybit_weight) + bybit * self.bybit_weight)
            }
            (Ok(only), Err(_)) | (Err(_), Ok(only)) => Ok(only),
            // Neither: say the most hopeful reason (warming up beats stale beats nothing).
            (Err(a), Err(b)) => Err(if a == NotReady::WarmingUp || b == NotReady::WarmingUp {
                NotReady::WarmingUp
            } else if a == NotReady::Stale || b == NotReady::Stale {
                NotReady::Stale
            } else {
                NotReady::NoPrice
            }),
        }
    }
}

impl Reference {
    fn on_price(&mut self, bid: Decimal, ask: Decimal, at: Instant) {
        if bid > Decimal::ZERO && ask >= bid {
            self.mid = Some(((bid + ask) / Decimal::TWO, at));
        }
    }

    fn on_venue_mid(
        &mut self,
        venue_mid: Decimal,
        now: Instant,
        stale: Duration,
        halflife: Duration,
    ) {
        let Ok(mid) = self.fresh_mid(now, stale) else {
            // Without a live reference price the gap means nothing; start over later.
            self.basis = None;
            return;
        };
        let sample = venue_mid - mid;
        self.basis = Some(match self.basis {
            None => Basis {
                average: sample,
                last_update: now,
                started: now,
            },
            Some(b) => {
                let weight = ema_weight(now - b.last_update, now - b.started, halflife);
                Basis {
                    average: (b.average + weight * (sample - b.average)).round_dp(8),
                    last_update: now,
                    started: b.started,
                }
            }
        });
    }

    /// This reference's fair value: its mid plus its basis, once warm.
    fn estimate(&self, now: Instant, stale: Duration) -> Result<Decimal, NotReady> {
        let mid = self.fresh_mid(now, stale)?;
        match self.basis {
            Some(b) if now.saturating_duration_since(b.started) >= WARMUP => Ok(mid + b.average),
            _ => Err(NotReady::WarmingUp),
        }
    }

    fn fresh_mid(&self, now: Instant, stale: Duration) -> Result<Decimal, NotReady> {
        match self.mid {
            None => Err(NotReady::NoPrice),
            Some((_, at)) if now.saturating_duration_since(at) > stale => Err(NotReady::Stale),
            Some((mid, _)) => Ok(mid),
        }
    }
}

/// How much a new sample moves the average. A sample `halflife` after the last
/// one moves it halfway. Early on (when we have little history) we take the
/// plain time-weighted mean instead, so the very first sample doesn't dominate.
fn ema_weight(since_last: Duration, since_start: Duration, halflife: Duration) -> Decimal {
    if since_last.is_zero() {
        return Decimal::ZERO;
    }
    let ema = 1.0 - 0.5_f64.powf(since_last.as_secs_f64() / halflife.as_secs_f64().max(1.0));
    let mean = since_last.as_secs_f64() / since_start.as_secs_f64();
    Decimal::from_f64(ema.max(mean).clamp(0.0, 1.0)).unwrap_or(Decimal::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE: Duration = Duration::from_secs(5);
    const HALFLIFE: Duration = Duration::from_secs(300);

    fn d(n: i64) -> Decimal {
        Decimal::from(n)
    }

    /// Binance mid 100, our exchange's mid 101, once a second for `secs` seconds.
    fn watched_for(secs: u64) -> (FairValue, Instant) {
        let start = Instant::now();
        let mut fv = FairValue::default();
        for s in 0..=secs {
            let t = start + Duration::from_secs(s);
            fv.on_binance(d(99), d(101), t);
            fv.on_venue_mid(d(101), t, STALE, HALFLIFE);
        }
        (fv, start + Duration::from_secs(secs))
    }

    /// As above, with Bybit too (mid 98, so its basis is 3), Bybit weight 0.5.
    fn both_for(secs: u64) -> (FairValue, Instant) {
        let start = Instant::now();
        let mut fv = FairValue::new(Decimal::new(5, 1));
        for s in 0..=secs {
            let t = start + Duration::from_secs(s);
            fv.on_binance(d(99), d(101), t);
            fv.on_bybit(d(97), d(99), t);
            fv.on_venue_mid(d(101), t, STALE, HALFLIFE);
        }
        (fv, start + Duration::from_secs(secs))
    }

    #[test]
    fn nothing_until_a_reference_speaks() {
        let fv = FairValue::default();
        assert_eq!(fv.get(Instant::now(), STALE), Err(NotReady::NoPrice));
    }

    #[test]
    fn waits_for_the_basis_to_warm_up() {
        let (fv, now) = watched_for(30);
        assert_eq!(fv.get(now, STALE), Err(NotReady::WarmingUp));
    }

    #[test]
    fn binance_mid_plus_basis_once_warm() {
        let (mut fv, now) = watched_for(60);
        assert_eq!(fv.get(now, STALE), Ok(d(101)));
        // Binance jumps 2: fair value follows straight away; the basis stays 1.
        fv.on_binance(d(101), d(103), now);
        assert_eq!(fv.get(now, STALE), Ok(d(103)));
    }

    #[test]
    fn stops_when_binance_goes_quiet() {
        let (fv, now) = watched_for(60);
        let later = now + STALE + Duration::from_secs(1);
        assert_eq!(fv.get(later, STALE), Err(NotReady::Stale));
    }

    #[test]
    fn each_reference_has_its_own_basis() {
        // Binance 100 and Bybit 98 both explain our exchange's 101: same fair value.
        let (fv, now) = both_for(60);
        assert_eq!(fv.get(now, STALE), Ok(d(101)));
    }

    #[test]
    fn blends_the_two_when_both_are_fresh() {
        let (mut fv, now) = both_for(60);
        // Binance jumps 2 (estimate 103), Bybit doesn't (101): half and half, 102.
        fv.on_binance(d(101), d(103), now);
        assert_eq!(fv.get(now, STALE), Ok(d(102)));
    }

    #[test]
    fn carries_on_with_bybit_when_binance_goes_quiet() {
        let (mut fv, now) = both_for(60);
        let later = now + STALE + Duration::from_secs(1);
        // Only Bybit keeps talking, and moves up 1.
        fv.on_bybit(d(98), d(100), later);
        assert_eq!(fv.get(later, STALE), Ok(d(102)), "Bybit's 99 + its basis 3");
    }

    #[test]
    fn a_quiet_bybit_changes_nothing() {
        // Never heard from Bybit: exactly the Binance-only fair value.
        let (mut fv, now) = watched_for(60);
        fv.bybit_weight = Decimal::new(5, 1);
        assert_eq!(fv.get(now, STALE), Ok(d(101)));
    }
}

//! The strategy: where to put our bid and our ask.
//!
//! A market maker earns the spread: buy a little below fair value, sell a
//! little above it, repeat. Three rules on top of that:
//!
//! 1. **Spread.** Quote `spread_bps` either side of fair value (the market
//!    maker may widen it when the market is jumpy, see `volatility.rs`).
//! 2. **Skew.** If we're long, lean both quotes down so we're more likely to
//!    sell (and less likely to buy more). Short: lean up. This keeps
//!    inventory near zero without ever crossing the spread to dump it.
//! 3. **Limits.** Don't quote the side that would take us past `max_position`,
//!    and never price a quote so it would cross the book (it would take,
//!    not make).

use rust_decimal::{Decimal, RoundingStrategy};

use crate::book::Book;
use crate::config::Config;

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// Which prices an exchange accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Ticks {
    /// Any multiple of this step, at every price (Bullet: 0.01 on SOL-USD).
    Fixed(Decimal),
    /// At most `figures` significant figures and `max_decimals` decimal
    /// places; whole numbers are always fine (Hyperliquid). So the step
    /// depends on the price: 0.001 at 99.5, but 0.01 at 100.5.
    SignificantFigures { figures: u32, max_decimals: u32 },
}

impl Ticks {
    /// The price step near `price`.
    pub fn at(&self, price: Decimal) -> Decimal {
        let (figures, max_decimals) = match *self {
            Ticks::Fixed(tick) => return tick,
            Ticks::SignificantFigures {
                figures,
                max_decimals,
            } => (figures, max_decimals),
        };
        let smallest = Decimal::new(1, max_decimals);
        if price <= Decimal::ZERO {
            return smallest;
        }
        // Which power of ten the first digit sits at: 2 for 150, -1 for 0.5.
        let (mut first_digit, mut p) = (0i64, price);
        while p >= Decimal::TEN {
            p /= Decimal::TEN;
            first_digit += 1;
        }
        while p < Decimal::ONE {
            p *= Decimal::TEN;
            first_digit -= 1;
        }
        // The last allowed figure sits `figures - 1` places further right.
        let last_digit = first_digit - (i64::from(figures) - 1);
        if last_digit >= 0 {
            Decimal::ONE // whole numbers are always allowed
        } else {
            Decimal::new(1, (-last_digit) as u32).max(smallest)
        }
    }
}

/// The prices we want resting. `None` = don't quote that side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Quotes {
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

pub fn quote(
    fair: Decimal,
    position: Decimal,
    config: &Config,
    book: &Book,
    ticks: Ticks,
    spread_bps: Decimal,
) -> Quotes {
    // How full our inventory is, from -1 (max short) to +1 (max long).
    let fullness = (position / config.max_position).clamp(Decimal::NEGATIVE_ONE, Decimal::ONE);
    let lean_bps = config.skew_bps * fullness;

    let bid = fair * (Decimal::ONE - (spread_bps + lean_bps) / BPS);
    let ask = fair * (Decimal::ONE + (spread_bps - lean_bps) / BPS);

    let room_to_buy = position + config.order_size <= config.max_position;
    let room_to_sell = position - config.order_size >= -config.max_position;

    Quotes {
        bid: room_to_buy.then(|| {
            // Stay at least one tick below the best ask, rounded down to a valid price.
            let bid = match book.best_ask() {
                Some(best_ask) => bid.min(best_ask - ticks.at(best_ask)),
                None => bid,
            };
            round_to_tick(bid, ticks.at(bid), RoundingStrategy::ToNegativeInfinity)
        }),
        ask: room_to_sell.then(|| {
            let ask = match book.best_bid() {
                Some(best_bid) => ask.max(best_bid + ticks.at(best_bid)),
                None => ask,
            };
            round_to_tick(ask, ticks.at(ask), RoundingStrategy::ToPositiveInfinity)
        }),
    }
}

fn round_to_tick(price: Decimal, tick: Decimal, rounding: RoundingStrategy) -> Decimal {
    if tick.is_zero() {
        return price;
    }
    (price / tick).round_dp_with_strategy(0, rounding) * tick
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// 10 bps spread, 10 bps skew, size 0.1, max position 1, tick 0.01.
    fn config() -> Config {
        let mut c = Config::load(Path::new("bot.toml")).unwrap();
        c.spread_bps = d("10");
        c.skew_bps = d("10");
        c.order_size = d("0.1");
        c.max_position = d("1");
        c
    }

    /// A wide book, so it never gets in the way unless a test wants it to.
    fn wide_book() -> Book {
        Book::from_levels(&[(d("90"), d("1"))], &[(d("110"), d("1"))])
    }

    const TICKS: Ticks = Ticks::Fixed(Decimal::from_parts(1, 0, 0, false, 2));

    /// Hyperliquid's rule for a coin with 2 size decimals (like SOL).
    const FIVE_FIGURES: Ticks = Ticks::SignificantFigures {
        figures: 5,
        max_decimals: 4,
    };

    #[test]
    fn a_fixed_tick_is_the_same_at_every_price() {
        assert_eq!(TICKS.at(d("0.5")), d("0.01"));
        assert_eq!(TICKS.at(d("150")), d("0.01"));
    }

    #[test]
    fn five_significant_figures_depend_on_the_price() {
        assert_eq!(FIVE_FIGURES.at(d("150.12")), d("0.01"), "150.12");
        assert_eq!(FIVE_FIGURES.at(d("99.5")), d("0.001"), "99.500");
        assert_eq!(FIVE_FIGURES.at(d("3000")), d("0.1"), "3000.0");
        assert_eq!(
            FIVE_FIGURES.at(d("2.5")),
            d("0.0001"),
            "max decimals caps it"
        );
        // Whole numbers are always allowed, however many figures they have.
        assert_eq!(FIVE_FIGURES.at(d("104321")), d("1"));
    }

    #[test]
    fn each_quote_rounds_to_the_tick_at_its_own_price() {
        // Fair value 100.02 with a 10 bps spread: the bid lands under 100
        // (steps of 0.001), the ask over it (steps of 0.01).
        let q = quote(
            d("100.02"),
            Decimal::ZERO,
            &config(),
            &wide_book(),
            FIVE_FIGURES,
            config().spread_bps,
        );
        assert_eq!(
            q,
            Quotes {
                bid: Some(d("99.919")),
                ask: Some(d("100.13"))
            }
        );
    }

    #[test]
    fn flat_quotes_spread_either_side() {
        let q = quote(
            d("100"),
            Decimal::ZERO,
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(
            q,
            Quotes {
                bid: Some(d("99.90")),
                ask: Some(d("100.10"))
            }
        );
    }

    #[test]
    fn long_leans_both_quotes_down() {
        // Half full: lean 5 bps.
        let q = quote(
            d("100"),
            d("0.5"),
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(
            q,
            Quotes {
                bid: Some(d("99.85")),
                ask: Some(d("100.05"))
            }
        );
    }

    #[test]
    fn short_leans_both_quotes_up() {
        let q = quote(
            d("100"),
            d("-0.5"),
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(
            q,
            Quotes {
                bid: Some(d("99.95")),
                ask: Some(d("100.15"))
            }
        );
    }

    #[test]
    fn stops_buying_at_max_long() {
        let q = quote(
            d("100"),
            d("1"),
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(q.bid, None);
        assert!(q.ask.is_some());
    }

    #[test]
    fn stops_selling_at_max_short() {
        let q = quote(
            d("100"),
            d("-1"),
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(q.ask, None);
        assert!(q.bid.is_some());
    }

    #[test]
    fn stops_buying_when_one_more_order_would_pass_the_max() {
        let q = quote(
            d("100"),
            d("0.95"),
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(q.bid, None);
    }

    #[test]
    fn never_crosses_the_book() {
        // Fair value says bid 99.90, but the best ask is 99.50.
        let book = Book::from_levels(&[(d("99.40"), d("1"))], &[(d("99.50"), d("1"))]);
        let q = quote(
            d("100"),
            Decimal::ZERO,
            &config(),
            &book,
            TICKS,
            config().spread_bps,
        );
        assert_eq!(q.bid, Some(d("99.49")), "one tick under the best ask");
        assert_eq!(q.ask, Some(d("100.10")));
    }

    #[test]
    fn quotes_the_spread_it_is_given() {
        // The market maker can widen the spread (volatility); 20 bps here.
        let q = quote(
            d("100"),
            Decimal::ZERO,
            &config(),
            &wide_book(),
            TICKS,
            d("20"),
        );
        assert_eq!(
            q,
            Quotes {
                bid: Some(d("99.80")),
                ask: Some(d("100.20"))
            }
        );
    }

    #[test]
    fn rounds_bids_down_and_asks_up_to_the_tick() {
        let q = quote(
            d("100.003"),
            Decimal::ZERO,
            &config(),
            &wide_book(),
            TICKS,
            config().spread_bps,
        );
        assert_eq!(
            q,
            Quotes {
                bid: Some(d("99.90")),
                ask: Some(d("100.11"))
            }
        );
    }
}

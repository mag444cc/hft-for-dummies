//! The market maker's brain: events in, intents out.
//!
//! This is a plain state machine. It never waits on the network and never
//! talks to an exchange, so everything it decides can be tested in a
//! millisecond with made-up events (see the tests at the bottom).
//!
//! It keeps track of what it *believes* rests on the book: one bid and one
//! ask at most. It updates that belief the moment it asks for a change, and
//! corrects it if the executor reports a failure.

use std::time::{Duration, Instant};

use rust_decimal::Decimal;

use crate::book::Book;
use crate::config::Config;
use crate::events::{Event, Fill, Intent, Order, Side};
use crate::fair_value::{FairValue, NotReady};
use crate::inventory::Inventory;
use crate::strategy::{self, Quotes, Ticks};
use crate::volatility::Volatility;

/// How much Binance history the volatility spread looks at.
const VOL_WINDOW: Duration = Duration::from_secs(60);

/// How often to check our fills against the exchange's record.
const FILL_CHECK_EVERY: Duration = Duration::from_secs(10);

pub struct MarketMaker {
    config: Config,
    /// Which prices the exchange accepts on this market.
    ticks: Ticks,
    fair: FairValue,
    /// How jumpy Binance is (only with `vol_spread_mult`).
    volatility: Option<Volatility>,
    book: Option<(Book, Instant)>,
    bid: Option<Order>,
    ask: Option<Order>,
    /// When we asked for the resting bid and ask; they expire `quote_ttl_secs` later.
    bid_since: Option<Instant>,
    ask_since: Option<Instant>,
    inventory: Inventory,
    next_client_id: u64,
    last_requote: Option<Instant>,
    last_summary: Option<Instant>,
    last_fill_check: Option<Instant>,
    /// Set when something means we must not quote again this run, and why.
    stopped: Option<&'static str>,
    /// Why we are (or aren't) quoting; logged when it changes.
    status: &'static str,
}

impl MarketMaker {
    pub fn new(config: Config, ticks: Ticks) -> Self {
        // Start ids from the clock so they never repeat across restarts.
        let next_client_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_millis() as u64 * 1000);
        // Moves over one requote gap: how far the price can go before we can follow.
        let volatility = config.vol_spread_mult.map(|_| {
            Volatility::new(
                Duration::from_millis(config.min_requote_ms.max(100)),
                VOL_WINDOW,
            )
        });
        Self {
            config,
            ticks,
            fair: FairValue::default(),
            volatility,
            book: None,
            bid: None,
            ask: None,
            bid_since: None,
            ask_since: None,
            inventory: Inventory::default(),
            next_client_id,
            last_requote: None,
            last_summary: None,
            last_fill_check: None,
            stopped: None,
            status: "",
        }
    }

    /// First thing on startup: clear out stale orders and learn our position.
    pub fn start(&mut self) -> Vec<Intent> {
        vec![Intent::CancelAll, Intent::SyncPosition]
    }

    /// Last thing before exiting: cancel everything, then catch up on any
    /// fill that landed just before the cancel did.
    pub fn stop(&mut self) -> Vec<Intent> {
        let mut intents = self.pull();
        intents.push(Intent::SyncFills);
        intents
    }

    /// Update what we know. Returns intents only when an event demands an
    /// immediate reaction; normal quoting happens in [`Self::requote`].
    pub fn on_event(&mut self, event: &Event, now: Instant) -> Vec<Intent> {
        match event {
            Event::Reference { bid, ask, stamp } => {
                self.fair.on_binance(*bid, *ask, stamp.received_at);
                if let Some(volatility) = &mut self.volatility {
                    volatility.on_price((*bid + *ask) / Decimal::TWO, stamp.received_at);
                }
            }
            Event::Book(book, _) => {
                if let Some(mid) = book.mid() {
                    self.fair.on_bullet_mid(
                        mid,
                        now,
                        self.reference_stale(),
                        self.basis_halflife(),
                    );
                    self.inventory.mark(mid);
                }
                self.book = Some((book.clone(), now));
            }
            Event::BookLost => self.book = None,
            Event::Fill(fill) => self.on_fill(fill),
            Event::OrderGone { client_id } => self.forget(*client_id),
            Event::PlaceFailed { client_ids } => client_ids.iter().for_each(|id| self.forget(*id)),
            Event::CancelFailed => {
                // An order we tried to cancel may still be resting. Cancel everything.
                tracing::warn!("a cancel failed; cancelling all our orders");
                return self.pull();
            }
            Event::Position(venue) => {
                if *venue != self.inventory.position {
                    tracing::warn!(ours = %self.inventory.position, exchange = %venue, "using the exchange's position");
                    self.inventory.adopt(*venue);
                }
                // The bot never holds more than max_position, so a bigger
                // position was opened by hand. Quoting would trade it down.
                if venue.abs() > self.config.max_position && self.stopped.is_none() {
                    tracing::error!(
                        "this account holds {venue} {}, more than max_position ({}). That isn't the \
                         bot's position, so it won't trade it. Close it, or raise max_position.",
                        self.config.symbol,
                        self.config.max_position
                    );
                    self.stopped =
                        Some("stopped: position bigger than max_position (opened by hand?)");
                    return self.pull();
                }
            }
            Event::Reconnecting => {
                // We may have missed fills or cancels: pull everything, catch up on fills.
                tracing::warn!("exchange reconnecting: pulling quotes and checking our fills");
                self.book = None;
                let mut intents = self.pull();
                intents.push(Intent::SyncFills);
                return intents;
            }
            Event::Tick => {
                if self
                    .last_summary
                    .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10))
                {
                    self.log_summary();
                    self.last_summary = Some(now);
                }
                if self
                    .last_fill_check
                    .is_none_or(|t| now.duration_since(t) >= FILL_CHECK_EVERY)
                {
                    self.last_fill_check = Some(now);
                    return vec![Intent::SyncFills];
                }
            }
            // Other people's trades only matter to the paper executor.
            Event::Trade { .. }
            | Event::Disconnected(_)
            | Event::Shutdown
            | Event::OrderLive { .. }
            | Event::Sent { .. } => {}
        }
        Vec::new()
    }

    /// Compare the quotes we want with what rests, and say what to change.
    pub fn requote(&mut self, now: Instant) -> Vec<Intent> {
        let (wanted, status) = self.wanted_quotes(now);
        if status != self.status {
            tracing::info!("{status}");
            self.status = status;
        }

        // Rate-limit replacing quotes, but never delay pulling them.
        let pulling = wanted.bid.is_none() && wanted.ask.is_none();
        let min_gap = Duration::from_millis(self.config.min_requote_ms);
        let waiting = !pulling
            && self
                .last_requote
                .is_some_and(|t| now.duration_since(t) < min_gap);

        // Replace a quote at half its lifetime, long before it expires.
        let refresh_after = Duration::from_secs(self.config.quote_ttl_secs) / 2;
        let mut intents = Vec::new();
        for (side, want) in [(Side::Buy, wanted.bid), (Side::Sell, wanted.ask)] {
            let resting = *self.slot(side);
            // While we wait out the gap, only a quote in the market's way moves.
            if waiting && !self.in_the_way(side, resting, want) {
                continue;
            }
            let young = self
                .since(side)
                .is_some_and(|t| now.duration_since(t) < refresh_after);
            let keep = match (resting, want) {
                (Some(order), Some(price)) => {
                    young && drift_bps(order.price, price) < self.config.requote_threshold_bps
                }
                (None, None) => true,
                _ => false,
            };
            if keep {
                continue;
            }
            let new = want.map(|price| Order {
                side,
                price,
                size: self.config.order_size,
                client_id: self.new_client_id(),
            });
            intents.push(match (resting, new) {
                // Moving a quote is one request: no moment with nothing on the book.
                (Some(old), Some(new)) => Intent::Replace {
                    old: old.client_id,
                    new,
                },
                (Some(old), None) => Intent::Cancel {
                    client_id: old.client_id,
                },
                (None, Some(new)) => Intent::Place(new),
                (None, None) => unreachable!("kept above"),
            });
            *self.slot(side) = new;
            *self.since(side) = Some(now);
        }
        if !intents.is_empty() {
            self.last_requote = Some(now);
        }
        intents
    }

    /// Has fair value (Binance) moved towards this resting order by at least
    /// `urgent_requote_bps`? Then the market is about to trade with a price we
    /// no longer want: a sell left too cheap, or a buy left too dear.
    fn in_the_way(&self, side: Side, resting: Option<Order>, want: Option<Decimal>) -> bool {
        let (Some(urgent), Some(order), Some(price)) =
            (self.config.urgent_requote_bps, resting, want)
        else {
            return false;
        };
        let towards = match side {
            Side::Buy => order.price > price,  // we'd now bid lower
            Side::Sell => order.price < price, // we'd now ask higher
        };
        towards && drift_bps(order.price, price) >= urgent
    }

    fn wanted_quotes(&self, now: Instant) -> (Quotes, &'static str) {
        let none = Quotes::default();
        if let Some(why) = self.stopped {
            return (none, why);
        }
        let Some((book, at)) = &self.book else {
            return (none, "waiting for the order book");
        };
        if now.duration_since(*at) > Duration::from_secs(self.config.book_stale_secs) {
            return (none, "order book is stale, not quoting");
        }
        match self.fair.get(now, self.reference_stale()) {
            Ok(fair) => (
                strategy::quote(
                    fair,
                    self.inventory.position,
                    &self.config,
                    book,
                    self.ticks,
                    self.spread_bps(),
                ),
                "quoting",
            ),
            Err(NotReady::NoBinancePrice) => (none, "waiting for Binance"),
            Err(NotReady::BinanceStale) => (none, "Binance price is stale, not quoting"),
            Err(NotReady::WarmingUp) => (
                none,
                "learning the price gap to Binance (takes about a minute)",
            ),
        }
    }

    fn on_fill(&mut self, fill: &Fill) {
        if !self.inventory.on_fill(fill) {
            return;
        }
        if fill.order_finished
            && let Some(id) = fill.client_id
        {
            self.forget(id);
        }
        let pnl = self.pnl();
        tracing::info!(
            "FILL: {:?} {} @ {} | position {} | pnl ${}",
            fill.side,
            fill.size,
            fill.price,
            self.inventory.position,
            show(pnl.map(|p| p.round_dp(4)))
        );
        if self.stopped.is_none() && pnl.is_some_and(|p| p <= -self.config.max_loss_usd) {
            tracing::error!(
                "loss limit of ${} hit: no more quoting this run",
                self.config.max_loss_usd
            );
            self.stopped = Some("stopped: loss limit hit");
        }
    }

    /// Cancel everything and forget it.
    fn pull(&mut self) -> Vec<Intent> {
        self.bid = None;
        self.ask = None;
        vec![Intent::CancelAll]
    }

    fn forget(&mut self, client_id: u64) {
        for side in [Side::Buy, Side::Sell] {
            let slot = self.slot(side);
            if slot.is_some_and(|o| o.client_id == client_id) {
                *slot = None;
            }
        }
    }

    fn slot(&mut self, side: Side) -> &mut Option<Order> {
        match side {
            Side::Buy => &mut self.bid,
            Side::Sell => &mut self.ask,
        }
    }

    fn since(&mut self, side: Side) -> &mut Option<Instant> {
        match side {
            Side::Buy => &mut self.bid_since,
            Side::Sell => &mut self.ask_since,
        }
    }

    fn new_client_id(&mut self) -> u64 {
        self.next_client_id += 1;
        self.next_client_id
    }

    fn mid(&self) -> Option<Decimal> {
        self.book.as_ref().and_then(|(book, _)| book.mid())
    }

    fn pnl(&self) -> Option<Decimal> {
        self.mid().map(|mid| self.inventory.pnl(mid))
    }

    fn reference_stale(&self) -> Duration {
        Duration::from_secs(self.config.reference_stale_secs)
    }

    fn basis_halflife(&self) -> Duration {
        Duration::from_secs(self.config.basis_halflife_secs)
    }

    /// The spread to quote: `spread_bps`, or wider if the market is jumpy.
    fn spread_bps(&self) -> Decimal {
        let jumpy = match (self.config.vol_spread_mult, &self.volatility) {
            (Some(mult), Some(volatility)) => volatility.move_bps().map(|m| m * mult),
            _ => None,
        };
        jumpy.map_or(self.config.spread_bps, |j| j.max(self.config.spread_bps))
    }

    pub fn log_summary(&self) {
        let fair = self.fair.get(Instant::now(), self.reference_stale()).ok();
        tracing::info!(
            "mid {} | fair {} | our bid {} ask {} | position {} | pnl ${} (fees ${})",
            show(self.mid()),
            show(fair.map(|f| f.round_dp(4))),
            show(self.bid.map(|o| o.price)),
            show(self.ask.map(|o| o.price)),
            self.inventory.position,
            show(self.pnl().map(|p| p.round_dp(4))),
            self.inventory.fees
        );
        if self.volatility.is_some() {
            tracing::info!("spread now {} bps", self.spread_bps().round_dp(2));
        }
    }
}

/// A number, or "-" when we don't have one yet.
fn show(value: Option<Decimal>) -> String {
    value.map_or_else(|| "-".to_string(), |v| v.to_string())
}

/// How far apart two prices are, in basis points.
fn drift_bps(a: Decimal, b: Decimal) -> Decimal {
    if b.is_zero() {
        return Decimal::MAX;
    }
    (a - b).abs() / b * Decimal::from(10_000)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::events::Stamp;
    use crate::fair_value::WARMUP;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// A market maker that has watched Binance at 100 and Bullet at 100 long
    /// enough to trust its fair value of 100. Returns it and the current time.
    fn warmed_up() -> (MarketMaker, Instant) {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        config.spread_bps = d("10");
        config.skew_bps = Decimal::ZERO;
        config.order_size = d("1");
        config.max_position = d("2");
        config.requote_threshold_bps = d("5");
        config.min_requote_ms = 0;
        let mut mm = MarketMaker::new(config, Ticks::Fixed(d("0.01")));
        let start = Instant::now();
        let book = Book::from_levels(&[(d("99.5"), d("10"))], &[(d("100.5"), d("10"))]);
        for s in 0..=WARMUP.as_secs() {
            let t = start + Duration::from_secs(s);
            mm.on_event(
                &Event::Reference {
                    bid: d("99.5"),
                    ask: d("100.5"),
                    stamp: Stamp::at(t),
                },
                t,
            );
            mm.on_event(&Event::Book(book.clone(), Stamp::at(t)), t);
        }
        (mm, start + WARMUP)
    }

    fn places(intents: &[Intent]) -> Vec<(Side, Decimal)> {
        intents
            .iter()
            .filter_map(|i| match i {
                Intent::Place(o) | Intent::Replace { new: o, .. } => Some((o.side, o.price)),
                _ => None,
            })
            .collect()
    }

    /// Binance's best bid and ask moved to these, `after` from `start`.
    fn binance(mm: &mut MarketMaker, bid: &str, ask: &str, at: Instant) {
        let event = Event::Reference {
            bid: d(bid),
            ask: d(ask),
            stamp: Stamp::at(at),
        };
        mm.on_event(&event, at);
    }

    /// Warmed up, quoting 99.90 / 100.10, and just requoted: the next
    /// ordinary requote has to wait 2 s.
    fn quoting_with_urgent(urgent: Option<&str>) -> (MarketMaker, Instant) {
        let (mut mm, now) = warmed_up();
        mm.config.min_requote_ms = 2000;
        mm.config.urgent_requote_bps = urgent.map(d);
        assert_eq!(
            places(&mm.requote(now)),
            vec![(Side::Buy, d("99.90")), (Side::Sell, d("100.10"))]
        );
        (mm, now)
    }

    #[test]
    fn a_binance_jump_up_moves_our_ask_at_once() {
        let (mut mm, now) = quoting_with_urgent(Some("5"));
        // Binance up 10 bps, 0.1 s after we quoted: our ask at 100.10 is now cheap.
        let soon = now + Duration::from_millis(100);
        binance(&mut mm, "99.6", "100.6", soon);
        assert_eq!(
            places(&mm.requote(soon)),
            vec![(Side::Sell, d("100.21"))],
            "only the side in the way moves; the bid can wait"
        );
    }

    #[test]
    fn a_binance_jump_down_moves_our_bid_at_once() {
        let (mut mm, now) = quoting_with_urgent(Some("5"));
        let soon = now + Duration::from_millis(100);
        binance(&mut mm, "99.4", "100.4", soon);
        assert_eq!(places(&mm.requote(soon)), vec![(Side::Buy, d("99.80"))]);
    }

    #[test]
    fn a_small_move_still_waits_for_the_requote_gap() {
        let (mut mm, now) = quoting_with_urgent(Some("5"));
        // Up 3 bps: past the 5 bps requote threshold? No. Urgent? No.
        let soon = now + Duration::from_millis(100);
        binance(&mut mm, "99.53", "100.53", soon);
        assert!(mm.requote(soon).is_empty());
    }

    #[test]
    fn without_urgent_requotes_a_jump_waits_too() {
        let (mut mm, now) = quoting_with_urgent(None);
        let soon = now + Duration::from_millis(100);
        binance(&mut mm, "99.6", "100.6", soon);
        assert!(mm.requote(soon).is_empty(), "the old behaviour");
        // Once the gap has passed, both sides move.
        let later = now + Duration::from_millis(2000);
        binance(&mut mm, "99.6", "100.6", later);
        assert_eq!(places(&mm.requote(later)).len(), 2);
    }

    /// Warmed up (1 s requote gap), then 60 s of Binance moving `swing` up
    /// and back every second, with the books following. Returns how far each
    /// quote then sits from their midpoint, in bps (half the bid-ask width).
    fn spread_after(swing: &str, vol_mult: Option<&str>) -> Decimal {
        let (mut mm, start) = warmed_up();
        mm.config.min_requote_ms = 1000;
        mm.config.vol_spread_mult = vol_mult.map(d);
        mm.volatility = mm
            .config
            .vol_spread_mult
            .map(|_| Volatility::new(Duration::from_secs(1), VOL_WINDOW));
        let mut now = start;
        for step in 0..600u64 {
            now = start + Duration::from_millis(step * 100);
            let mid = if (step / 10) % 2 == 0 {
                d("100")
            } else {
                d("100") + d(swing)
            };
            let half = d("0.5");
            mm.on_event(
                &Event::Reference {
                    bid: mid - half,
                    ask: mid + half,
                    stamp: Stamp::at(now),
                },
                now,
            );
            let book = Book::from_levels(&[(mid - half, d("10"))], &[(mid + half, d("10"))]);
            mm.on_event(&Event::Book(book, Stamp::at(now)), now);
        }
        let intents = mm.requote(now + Duration::from_secs(2));
        let p = places(&intents);
        let (bid, ask) = (p[0].1, p[1].1);
        ((ask - bid) / (ask + bid) * Decimal::from(10_000)).round_dp(1)
    }

    #[test]
    fn a_jumpy_market_widens_the_spread() {
        // Binance swings 20 bps a second; spread at least 1 x that typical move.
        let spread = spread_after("0.2", Some("1"));
        assert!(spread >= d("19"), "quoted {spread} bps, expected about 20");
    }

    #[test]
    fn a_calm_market_keeps_the_minimum_spread() {
        // Swings of 1 bp: 1 x 1 bp is below spread_bps (10), so 10 it is
        // (give or take the rounding to the 0.01 price step).
        let spread = spread_after("0.01", Some("1"));
        assert!(
            spread >= d("9.5") && spread <= d("11"),
            "quoted {spread} bps"
        );
    }

    #[test]
    fn without_vol_spread_mult_the_spread_is_fixed() {
        let spread = spread_after("0.2", None);
        assert!(
            spread >= d("9.5") && spread <= d("11"),
            "quoted {spread} bps"
        );
    }

    #[test]
    fn shutdown_cancels_everything_then_checks_fills() {
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        assert_eq!(mm.stop(), vec![Intent::CancelAll, Intent::SyncFills]);
    }

    #[test]
    fn startup_clears_old_orders_and_reads_the_position() {
        let (mut mm, _) = warmed_up();
        assert_eq!(mm.start(), vec![Intent::CancelAll, Intent::SyncPosition]);
    }

    #[test]
    fn quotes_both_sides_once_warmed_up() {
        let (mut mm, now) = warmed_up();
        assert_eq!(
            places(&mm.requote(now)),
            vec![(Side::Buy, d("99.90")), (Side::Sell, d("100.10"))]
        );
        assert!(mm.requote(now).is_empty(), "nothing changed, nothing to do");
    }

    #[test]
    fn does_not_quote_before_the_basis_is_learned() {
        let (mut mm, now) = warmed_up();
        mm.fair = FairValue::default();
        mm.on_event(
            &Event::Reference {
                bid: d("99.5"),
                ask: d("100.5"),
                stamp: Stamp::at(now),
            },
            now,
        );
        assert!(mm.requote(now).is_empty());
    }

    #[test]
    fn small_moves_keep_the_quote_big_moves_replace_it() {
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        // Binance up 2 bps: under the 5 bps threshold, keep.
        mm.on_event(
            &Event::Reference {
                bid: d("99.52"),
                ask: d("100.52"),
                stamp: Stamp::at(now),
            },
            now,
        );
        assert!(mm.requote(now).is_empty());
        // Up 10 bps: cancel both, place both.
        mm.on_event(
            &Event::Reference {
                bid: d("99.6"),
                ask: d("100.6"),
                stamp: Stamp::at(now),
            },
            now,
        );
        let intents = mm.requote(now);
        assert!(
            intents.iter().all(|i| matches!(i, Intent::Replace { .. })),
            "moved quotes are replaced in one request: {intents:?}"
        );
        // Fair 100.10: bid 99.9999 rounds down, ask 100.2001 rounds up.
        assert_eq!(
            places(&intents),
            vec![(Side::Buy, d("99.99")), (Side::Sell, d("100.21"))]
        );
    }

    #[test]
    fn refreshes_quotes_before_they_expire() {
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        let half_life = Duration::from_secs(mm.config.quote_ttl_secs) / 2;
        // The market doesn't move, but keeps ticking so nothing goes stale.
        let book = mm.book.clone().unwrap().0;
        let tick = |mm: &mut MarketMaker, t: Instant| {
            mm.on_event(
                &Event::Reference {
                    bid: d("99.5"),
                    ask: d("100.5"),
                    stamp: Stamp::at(t),
                },
                t,
            );
            mm.on_event(&Event::Book(book.clone(), Stamp::at(t)), t);
        };
        let young = now + half_life - Duration::from_secs(1);
        tick(&mut mm, young);
        assert!(mm.requote(young).is_empty(), "still young: keep");
        tick(&mut mm, now + half_life);
        let intents = mm.requote(now + half_life);
        assert_eq!(
            places(&intents),
            vec![(Side::Buy, d("99.90")), (Side::Sell, d("100.10"))],
            "same prices, fresh orders"
        );
        assert!(
            intents.iter().all(|i| matches!(i, Intent::Replace { .. })),
            "{intents:?}"
        );
    }

    #[test]
    fn pulls_quotes_when_binance_goes_quiet() {
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        let later = now + Duration::from_secs(mm.config.reference_stale_secs + 1);
        // Bullet keeps updating, Binance doesn't.
        let book = mm.book.clone().unwrap().0;
        mm.on_event(&Event::Book(book, Stamp::at(later)), later);
        let intents = mm.requote(later);
        assert_eq!(intents.len(), 2);
        assert!(
            intents.iter().all(|i| matches!(i, Intent::Cancel { .. })),
            "{intents:?}"
        );
    }

    #[test]
    fn a_fill_moves_the_position_and_the_filled_side_is_requoted() {
        let (mut mm, now) = warmed_up();
        let intents = mm.requote(now);
        let Intent::Place(bid) = intents[0] else {
            panic!()
        };
        mm.on_event(
            &Event::Fill(Fill {
                trade_id: 1,
                client_id: Some(bid.client_id),
                side: Side::Buy,
                price: bid.price,
                size: d("1"),
                fee: Decimal::ZERO,
                order_finished: true,
            }),
            now,
        );
        assert_eq!(mm.inventory.position, d("1"));
        assert_eq!(
            places(&mm.requote(now)),
            vec![(Side::Buy, d("99.90"))],
            "only the filled side is replaced"
        );
    }

    #[test]
    fn hitting_the_loss_limit_pulls_quotes_for_good() {
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        // Bought 1 at 110 with the mid at 100: down $10, limit is $2.
        mm.on_event(
            &Event::Fill(Fill {
                trade_id: 1,
                client_id: None,
                side: Side::Buy,
                price: d("110"),
                size: d("1"),
                fee: Decimal::ZERO,
                order_finished: false,
            }),
            now,
        );
        let intents = mm.requote(now);
        assert!(intents.iter().all(|i| matches!(i, Intent::Cancel { .. })) && !intents.is_empty());
        assert!(
            mm.requote(now + Duration::from_secs(60)).is_empty(),
            "and never quotes again"
        );
    }

    #[test]
    fn a_failed_place_is_forgotten_and_retried() {
        let (mut mm, now) = warmed_up();
        let intents = mm.requote(now);
        let ids = intents
            .iter()
            .filter_map(|i| {
                if let Intent::Place(o) = i {
                    Some(o.client_id)
                } else {
                    None
                }
            })
            .collect();
        mm.on_event(&Event::PlaceFailed { client_ids: ids }, now);
        assert_eq!(places(&mm.requote(now)).len(), 2);
    }

    #[test]
    fn adopts_a_small_position_from_the_exchange() {
        let (mut mm, now) = warmed_up();
        assert!(mm.on_event(&Event::Position(d("1")), now).is_empty());
        assert_eq!(mm.inventory.position, d("1"));
        assert!(!mm.requote(now).is_empty(), "still quotes");
    }

    #[test]
    fn refuses_to_trade_a_position_opened_by_hand() {
        // max_position is 2; the account already holds 5 (someone traded by hand).
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        assert_eq!(
            mm.on_event(&Event::Position(d("5")), now),
            vec![Intent::CancelAll]
        );
        assert!(mm.requote(now).is_empty(), "never sells it down");
        assert!(mm.requote(now + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn checks_fills_against_the_exchange_every_ten_seconds() {
        let (mut mm, now) = warmed_up();
        assert_eq!(mm.on_event(&Event::Tick, now), vec![Intent::SyncFills]);
        assert!(
            mm.on_event(&Event::Tick, now + Duration::from_secs(5))
                .is_empty()
        );
        assert_eq!(
            mm.on_event(&Event::Tick, now + FILL_CHECK_EVERY),
            vec![Intent::SyncFills]
        );
    }

    #[test]
    fn a_fill_the_websocket_missed_is_recovered_once() {
        let (mut mm, now) = warmed_up();
        let fill = Fill {
            trade_id: 42,
            client_id: None,
            side: Side::Sell,
            price: d("100"),
            size: d("1"),
            fee: Decimal::ZERO,
            order_finished: false,
        };
        // The exchange's record brings it in; a late websocket copy changes nothing.
        mm.on_event(&Event::Fill(fill.clone()), now);
        mm.on_event(&Event::Fill(fill), now);
        assert_eq!(mm.inventory.position, d("-1"));
    }

    #[test]
    fn a_reconnect_pulls_everything_and_checks_fills() {
        let (mut mm, now) = warmed_up();
        mm.requote(now);
        assert_eq!(
            mm.on_event(&Event::Reconnecting, now),
            vec![Intent::CancelAll, Intent::SyncFills]
        );
        assert!(
            mm.requote(now).is_empty(),
            "no book until the next snapshot"
        );
    }
}

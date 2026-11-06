//! Executors carry out the strategy's intents and report back as events.
//!
//! - [`LiveExecutor`] sends real orders to an exchange (a [`Venue`]: Bullet or
//!   Hyperliquid).
//! - [`PaperExecutor`] pretends to (dry run): it remembers the orders and
//!   fills them when the real order book trades through their price.
//!
//! The strategy can't tell the two apart, so what you see in dry run is the
//! same code path that trades live.
//!
//! Handing intents to an executor never waits: the live one passes them to a
//! worker task and returns at once, so the bot keeps reading prices while a
//! request to the exchange is in flight. Results come back as events.

use std::time::Instant;

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::book::Book;
use crate::events::{Bus, Event, Fill, Intent, Order, SentKind, Side};

pub trait Executor {
    /// Queue `intents`. Returns immediately.
    fn execute(&mut self, intents: Vec<Intent>);

    /// See every event too (the paper executor uses the book to fill orders).
    fn on_event(&mut self, _event: &Event) {}

    /// Wait for everything queued to be done (on shutdown).
    async fn finish(self);
}

/// The order changes from one batch of intents, sent together.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Orders to cancel, by client id.
    pub cancels: Vec<u64>,
    /// Resting orders (by client id) to swap for new ones.
    pub replaces: Vec<(u64, Order)>,
    /// New orders.
    pub places: Vec<Order>,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.cancels.is_empty() && self.replaces.is_empty() && self.places.is_empty()
    }

    /// Client ids of every new order in the batch.
    fn new_ids(&self) -> Vec<u64> {
        let replaced = self.replaces.iter().map(|(_, new)| new.client_id);
        replaced
            .chain(self.places.iter().map(|o| o.client_id))
            .collect()
    }
}

/// What the exchange turned down in a request that otherwise went through.
/// Bullet takes a request whole or not at all; Hyperliquid answers order by
/// order, so one order can be rejected while the rest rest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// New orders that are not resting.
    pub client_ids: Vec<u64>,
    /// A cancel (or the old half of a replace) failed: an order may still rest.
    pub cancel_failed: bool,
}

/// An exchange the bot can trade on. Each one (`bullet.rs`, `hyperliquid.rs`)
/// turns these into its own requests. The `+ Send` lets the worker run on
/// tokio's thread pool.
pub trait Venue: Send + 'static {
    /// Send one batch of cancels, replaces and new orders (all post-only).
    /// `Err` means none of it can be trusted to have happened.
    fn submit(&mut self, batch: &Batch) -> impl Future<Output = Result<Rejected>> + Send;
    /// Cancel every order we have on this market, tracked or not.
    fn cancel_all(&mut self) -> impl Future<Output = Result<()>> + Send;
    /// Our position according to the exchange (positive = long).
    fn position(&mut self) -> impl Future<Output = Result<Decimal>> + Send;
    /// Our fills on this market since `since_unix_us`, from the exchange's own record.
    fn fills_since(&mut self, since_unix_us: u64)
    -> impl Future<Output = Result<Vec<Fill>>> + Send;
}

/// Sends intents to a worker task that talks to the exchange, one batch at a time.
pub struct LiveExecutor {
    queue: mpsc::UnboundedSender<Vec<Intent>>,
    worker: JoinHandle<()>,
}

impl LiveExecutor {
    pub fn new(venue: impl Venue, bus: Bus) -> Self {
        let (queue, mut batches) = mpsc::unbounded_channel::<Vec<Intent>>();
        let mut worker = Worker {
            venue,
            bus,
            fills_since_us: None,
        };
        let worker = tokio::spawn(async move {
            // One batch at a time, in order: a cancel always lands before the
            // order that follows it.
            while let Some(batch) = batches.recv().await {
                worker.run(batch).await;
            }
        });
        Self { queue, worker }
    }
}

impl Executor for LiveExecutor {
    fn execute(&mut self, intents: Vec<Intent>) {
        if !intents.is_empty() {
            let _ = self.queue.send(intents);
        }
    }

    async fn finish(self) {
        drop(self.queue); // no more work: the worker stops once the queue is empty
        let _ = self.worker.await;
    }
}

struct Worker<V> {
    venue: V,
    bus: Bus,
    /// Fills from this time on (µs since 1970) are ours to count. Set when we
    /// read the starting position: everything before is already in it.
    fills_since_us: Option<u64>,
}

impl<V: Venue> Worker<V> {
    async fn run(&mut self, intents: Vec<Intent>) {
        let mut batch = Batch::default();
        for intent in intents {
            match intent {
                Intent::Place(order) => batch.places.push(order),
                Intent::Replace { old, new } => batch.replaces.push((old, new)),
                Intent::Cancel { client_id } => batch.cancels.push(client_id),
                Intent::CancelAll => {
                    if let Err(e) = self.venue.cancel_all().await {
                        tracing::error!(
                            "cancel all failed, check the exchange's app for resting orders: {e}"
                        );
                    }
                }
                Intent::SyncPosition => match self.venue.position().await {
                    Ok(position) => {
                        // Fills before this read are already in `position`. Our
                        // clock may be off from the exchange's, so start counting
                        // 10s later: nothing of ours can fill that early anyway,
                        // since we cancelled everything and the bot spends its
                        // first minute warming up before it quotes.
                        self.fills_since_us
                            .get_or_insert_with(|| unix_us() + 10_000_000);
                        self.send(Event::Position(position));
                    }
                    Err(e) => tracing::error!("reading our position failed: {e}"),
                },
                Intent::SyncFills => {
                    let Some(since) = self.fills_since_us else {
                        continue;
                    };
                    match self.venue.fills_since(since).await {
                        Ok(fills) => fills.into_iter().for_each(|f| self.send(Event::Fill(f))),
                        Err(e) => tracing::warn!("checking our fills failed (will retry): {e}"),
                    }
                }
            }
        }
        if batch.is_empty() {
            return;
        }
        // Everything goes out together: cancels (including the old side of
        // each moved quote) and new orders. A cancel for an order that filled
        // just before it arrived is skipped by the exchange, so that race
        // needs no special handling.
        let kind = match (batch.replaces.is_empty(), batch.places.is_empty()) {
            (false, _) => SentKind::Replace,
            (true, false) => SentKind::Place,
            (true, true) => SentKind::Cancel,
        };
        let started = Instant::now();
        match self.venue.submit(&batch).await {
            Ok(rejected) => {
                self.send(Event::Sent {
                    kind,
                    started,
                    finished: Instant::now(),
                });
                if rejected.cancel_failed {
                    tracing::warn!("the exchange refused a cancel");
                    self.send(Event::CancelFailed);
                }
                if !rejected.client_ids.is_empty() {
                    self.send(Event::PlaceFailed {
                        client_ids: rejected.client_ids,
                    });
                }
            }
            Err(e) => {
                // Nothing in the request happened, but we may not have heard back
                // from a request that did. Tell the strategy, which cancels
                // everything and starts its quotes afresh.
                tracing::warn!("sending orders failed (often: not enough margin): {e}");
                self.send(Event::CancelFailed);
                self.send(Event::PlaceFailed {
                    client_ids: batch.new_ids(),
                });
            }
        }
    }

    fn send(&self, event: Event) {
        let _ = self.bus.send(event);
    }
}

fn unix_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Dry run: orders live only in memory, and fill the way a real one would.
///
/// A real order joins the back of the queue at its price, and fills only when
/// someone trades at that price, after everyone ahead of it. So:
///
/// - Placing an order, we note how much already rests at our price: the
///   queue *ahead* of us.
/// - When the book shows less resting there (some left), we move up.
/// - A trade at our price uses up the queue ahead first; whatever is left of
///   it fills us, maybe only in part.
/// - A trade, or the book, going *through* our price means everyone at our
///   price was filled, us too.
///
/// Without a feed of the market's trades (Bullet), only that last rule
/// fills. Still a little kind to us: our orders arrive and leave instantly.
pub struct PaperExecutor {
    resting: Vec<PaperOrder>,
    /// The latest book, to see the queue at a price when we join it.
    book: Option<Book>,
    bus: Bus,
    next_trade_id: u64,
    /// Fee per fill, as a fraction of its value (bps / 10,000).
    fee_rate: Decimal,
}

struct PaperOrder {
    order: Order,
    /// Not filled yet.
    left: Decimal,
    /// Others resting at our price before us.
    ahead: Decimal,
    /// When we placed it (µs since 1970): earlier trades can't fill it.
    placed_unix_us: u64,
}

impl PaperExecutor {
    pub fn new(bus: Bus, fee_bps: Decimal) -> Self {
        Self {
            resting: Vec::new(),
            book: None,
            bus,
            next_trade_id: 0,
            fee_rate: fee_bps / Decimal::from(10_000),
        }
    }

    /// Join the back of the queue at the order's price.
    fn rest(&mut self, order: Order) {
        let ahead = self.book.as_ref().map_or(Decimal::ZERO, |book| {
            queue_at(book, order.side, order.price)
        });
        self.resting.push(PaperOrder {
            order,
            left: order.size,
            ahead,
            placed_unix_us: unix_us(),
        });
    }

    /// Fill `size` of resting order `i`, and report it like the exchange would.
    fn fill(&mut self, i: usize, size: Decimal) {
        let paper = &mut self.resting[i];
        let size = size.min(paper.left);
        if size <= Decimal::ZERO {
            return;
        }
        paper.left -= size;
        self.next_trade_id += 1;
        let order = paper.order;
        let _ = self.bus.send(Event::Fill(Fill {
            trade_id: self.next_trade_id,
            client_id: Some(order.client_id),
            side: order.side,
            price: order.price,
            size,
            fee: (order.price * size * self.fee_rate).round_dp(8),
            order_finished: paper.left.is_zero(),
        }));
    }
}

/// How much others have resting at `price` on our side of the book.
fn queue_at(book: &Book, side: Side, price: Decimal) -> Decimal {
    let level = match side {
        Side::Buy => book.bids.get(&price),
        Side::Sell => book.asks.get(&price),
    };
    level.copied().unwrap_or(Decimal::ZERO)
}

impl Executor for PaperExecutor {
    async fn finish(self) {}

    fn execute(&mut self, intents: Vec<Intent>) {
        for intent in intents {
            match intent {
                Intent::Place(order) => {
                    tracing::info!(side = ?order.side, price = %order.price, size = %order.size, "paper: place");
                    self.rest(order);
                }
                Intent::Replace { old, new } => {
                    tracing::info!(side = ?new.side, price = %new.price, size = %new.size, "paper: replace");
                    self.resting.retain(|o| o.order.client_id != old);
                    self.rest(new);
                }
                Intent::Cancel { client_id } => {
                    self.resting.retain(|o| o.order.client_id != client_id)
                }
                Intent::CancelAll => self.resting.clear(),
                // Paper trading starts flat and only changes by paper fills.
                Intent::SyncPosition | Intent::SyncFills => {}
            }
        }
    }

    fn on_event(&mut self, event: &Event) {
        match event {
            Event::Book(book, _) => {
                let (best_bid, best_ask) = (book.best_bid(), book.best_ask());
                for i in 0..self.resting.len() {
                    let o = &mut self.resting[i];
                    // Fewer resting ahead of us than we thought: some left.
                    o.ahead = o.ahead.min(queue_at(book, o.order.side, o.order.price));
                    let through = match o.order.side {
                        Side::Buy => best_ask.is_some_and(|ask| ask < o.order.price),
                        Side::Sell => best_bid.is_some_and(|bid| bid > o.order.price),
                    };
                    if through {
                        let left = o.left;
                        self.fill(i, left);
                    }
                }
                self.book = Some(book.clone());
            }
            Event::Trade { price, size, stamp } => {
                for i in 0..self.resting.len() {
                    let o = &mut self.resting[i];
                    // A trade from before our order existed (the feed replays
                    // recent trades when it connects) can't have filled it.
                    if stamp
                        .exchange_unix_us
                        .is_some_and(|at| at < o.placed_unix_us)
                    {
                        continue;
                    }
                    let (ours, left) = (o.order.price, o.left);
                    let through = match o.order.side {
                        Side::Buy => *price < ours,
                        Side::Sell => *price > ours,
                    };
                    if through {
                        self.fill(i, left);
                    } else if *price == ours {
                        // The trade uses up the queue ahead of us first.
                        let past_queue = *size - o.ahead;
                        o.ahead = (o.ahead - *size).max(Decimal::ZERO);
                        self.fill(i, past_queue);
                    }
                }
            }
            _ => return,
        }
        self.resting.retain(|o| !o.left.is_zero());
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use tokio::sync::mpsc;

    use super::*;
    use crate::events::Stamp;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn book_event(book: Book) -> Event {
        Event::Book(book, Stamp::at(Instant::now()))
    }

    fn trade(price: &str, size: &str) -> Event {
        Event::Trade {
            price: d(price),
            size: d(size),
            stamp: Stamp::at(Instant::now()),
        }
    }

    fn bid(client_id: u64, price: &str, size: &str) -> Order {
        Order {
            side: Side::Buy,
            price: d(price),
            size: d(size),
            client_id,
        }
    }

    /// Every fill reported so far, as (client id, size, finished).
    fn fills(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<(Option<u64>, Decimal, bool)> {
        let mut out = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let Event::Fill(f) = event {
                out.push((f.client_id, f.size, f.order_finished));
            }
        }
        out
    }

    #[tokio::test]
    async fn paper_fills_when_the_book_goes_through_our_price() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        paper.execute(vec![Intent::Place(bid(7, "100", "1"))]);

        // Best ask 100.5: above our bid, nothing happens.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("99"), d("1"))],
            &[(d("100.5"), d("1"))],
        )));
        // Best ask 100, our price: sellers are there, but nobody traded with us.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("99"), d("1"))],
            &[(d("100"), d("1"))],
        )));
        assert!(fills(&mut events).is_empty(), "touching isn't filling");

        // Best ask 99.9, below our bid: we must have been sold to.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("99"), d("1"))],
            &[(d("99.9"), d("1"))],
        )));
        assert_eq!(fills(&mut events), vec![(Some(7), d("1"), true)]);

        // Filled orders are gone.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("99"), d("1"))],
            &[(d("99.5"), d("1"))],
        )));
        assert!(fills(&mut events).is_empty());
    }

    #[tokio::test]
    async fn paper_waits_its_turn_behind_the_queue() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        // 5 already bid at 100 when we join: we're behind them.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("100"), d("5"))],
            &[(d("100.5"), d("1"))],
        )));
        paper.execute(vec![Intent::Place(bid(7, "100", "2"))]);

        paper.on_event(&trade("100", "4"));
        assert!(fills(&mut events).is_empty(), "4 of the 5 ahead filled");
        paper.on_event(&trade("100", "2"));
        assert_eq!(
            fills(&mut events),
            vec![(Some(7), d("1"), false)],
            "the last 1 ahead, then 1 of our 2"
        );
        paper.on_event(&trade("100", "3"));
        assert_eq!(fills(&mut events), vec![(Some(7), d("1"), true)]);
    }

    #[tokio::test]
    async fn paper_moves_up_when_those_ahead_leave() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        paper.on_event(&book_event(Book::from_levels(
            &[(d("100"), d("5"))],
            &[(d("100.5"), d("1"))],
        )));
        paper.execute(vec![Intent::Place(bid(7, "100", "1"))]);
        // Only 1 still bids at 100 (besides us): 4 ahead of us cancelled.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("100"), d("1"))],
            &[(d("100.5"), d("1"))],
        )));
        paper.on_event(&trade("100", "1.5"));
        assert_eq!(fills(&mut events), vec![(Some(7), d("0.5"), false)]);
    }

    #[tokio::test]
    async fn paper_fills_whole_on_a_trade_through_and_charges_the_fee() {
        let (bus, mut events) = mpsc::unbounded_channel();
        // 1.5 bps, Hyperliquid's base maker fee.
        let mut paper = PaperExecutor::new(bus, d("1.5"));
        paper.on_event(&book_event(Book::from_levels(
            &[(d("100"), d("50"))],
            &[(d("100.5"), d("1"))],
        )));
        paper.execute(vec![Intent::Place(bid(7, "100", "2"))]);
        // A sale at 99.9 went past everyone bidding 100, us included.
        paper.on_event(&trade("99.9", "0.1"));
        let Ok(Event::Fill(fill)) = events.try_recv() else {
            panic!("expected a fill")
        };
        assert_eq!((fill.size, fill.order_finished), (d("2"), true));
        assert_eq!(fill.fee, d("0.03"), "100 * 2 * 0.00015");
    }

    #[tokio::test]
    async fn paper_ignores_trades_from_before_the_order() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        paper.execute(vec![Intent::Place(bid(7, "100", "1"))]);
        // A replayed trade from a minute ago, straight through our price.
        let mut old = Stamp::at(Instant::now());
        old.exchange_unix_us = Some(unix_us() - 60_000_000);
        paper.on_event(&Event::Trade {
            price: d("99"),
            size: d("10"),
            stamp: old,
        });
        assert!(fills(&mut events).is_empty());
    }

    #[tokio::test]
    async fn paper_ignores_trades_away_from_our_price() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        paper.execute(vec![Intent::Place(bid(7, "100", "1"))]);
        paper.on_event(&trade("100.2", "10"));
        assert!(fills(&mut events).is_empty());
    }

    #[tokio::test]
    async fn paper_replace_moves_the_order() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        let moved = bid(2, "98", "1");
        paper.execute(vec![
            Intent::Place(bid(1, "100", "1")),
            Intent::Replace { old: 1, new: moved },
        ]);
        // Asks down to 99: would have filled the old bid at 100, not the moved one at 98.
        paper.on_event(&book_event(Book::from_levels(
            &[(d("97"), d("1"))],
            &[(d("99"), d("1"))],
        )));
        assert!(fills(&mut events).is_empty());
        paper.on_event(&book_event(Book::from_levels(
            &[(d("97"), d("1"))],
            &[(d("97.9"), d("1"))],
        )));
        assert_eq!(fills(&mut events), vec![(Some(2), d("1"), true)]);
    }

    #[tokio::test]
    async fn paper_cancels_remove_orders() {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut paper = PaperExecutor::new(bus, Decimal::ZERO);
        let ask = Order {
            side: Side::Sell,
            price: d("101"),
            size: d("1"),
            client_id: 8,
        };
        paper.execute(vec![Intent::Place(ask), Intent::Cancel { client_id: 8 }]);
        paper.on_event(&book_event(Book::from_levels(
            &[(d("102"), d("1"))],
            &[(d("103"), d("1"))],
        )));
        assert!(events.try_recv().is_err());
    }

    /// An exchange that records each batch and answers as the test says.
    struct FakeVenue {
        sent: Vec<Batch>,
        answer: fn() -> Result<Rejected>,
    }

    impl Venue for FakeVenue {
        async fn submit(&mut self, batch: &Batch) -> Result<Rejected> {
            self.sent.push(batch.clone());
            (self.answer)()
        }
        async fn cancel_all(&mut self) -> Result<()> {
            Ok(())
        }
        async fn position(&mut self) -> Result<Decimal> {
            Ok(Decimal::ZERO)
        }
        async fn fills_since(&mut self, _since_unix_us: u64) -> Result<Vec<Fill>> {
            Ok(Vec::new())
        }
    }

    fn order(client_id: u64, side: Side, price: &str) -> Order {
        Order {
            side,
            price: d(price),
            size: d("1"),
            client_id,
        }
    }

    /// Run `intents` through a worker; return what reached the exchange and
    /// the events reported back (without the timing ones).
    async fn run(
        intents: Vec<Intent>,
        answer: fn() -> Result<Rejected>,
    ) -> (Vec<Batch>, Vec<Event>) {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut worker = Worker {
            venue: FakeVenue {
                sent: Vec::new(),
                answer,
            },
            bus,
            fills_since_us: None,
        };
        worker.run(intents).await;
        let mut reported = Vec::new();
        while let Ok(event) = events.try_recv() {
            if !matches!(event, Event::Sent { .. }) {
                reported.push(event);
            }
        }
        (worker.venue.sent, reported)
    }

    #[tokio::test]
    async fn one_batch_of_intents_is_one_request() {
        let bid = order(2, Side::Buy, "99");
        let ask = order(3, Side::Sell, "101");
        let (sent, events) = run(
            vec![
                Intent::Replace { old: 1, new: bid },
                Intent::Place(ask),
                Intent::Cancel { client_id: 4 },
            ],
            || Ok(Rejected::default()),
        )
        .await;
        assert_eq!(
            sent,
            vec![Batch {
                cancels: vec![4],
                replaces: vec![(1, bid)],
                places: vec![ask],
            }]
        );
        assert!(events.is_empty(), "all went through: {events:?}");
    }

    #[tokio::test]
    async fn a_failed_request_pulls_everything() {
        let (_, events) = run(
            vec![
                Intent::Replace {
                    old: 1,
                    new: order(2, Side::Buy, "99"),
                },
                Intent::Place(order(3, Side::Sell, "101")),
            ],
            || Err(anyhow::anyhow!("timed out")),
        )
        .await;
        assert!(matches!(events[0], Event::CancelFailed));
        let Event::PlaceFailed { client_ids } = &events[1] else {
            panic!("{events:?}")
        };
        assert_eq!(client_ids, &vec![2, 3]);
    }

    #[tokio::test]
    async fn a_rejected_order_is_forgotten_and_nothing_else() {
        let (_, events) = run(
            vec![
                Intent::Place(order(2, Side::Buy, "99")),
                Intent::Place(order(3, Side::Sell, "101")),
            ],
            || {
                Ok(Rejected {
                    client_ids: vec![3],
                    cancel_failed: false,
                })
            },
        )
        .await;
        assert_eq!(events.len(), 1, "{events:?}");
        let Event::PlaceFailed { client_ids } = &events[0] else {
            panic!("{events:?}")
        };
        assert_eq!(client_ids, &vec![3]);
    }

    #[tokio::test]
    async fn a_refused_cancel_pulls_everything() {
        let (_, events) = run(vec![Intent::Cancel { client_id: 1 }], || {
            Ok(Rejected {
                client_ids: Vec::new(),
                cancel_failed: true,
            })
        })
        .await;
        assert!(
            matches!(events.as_slice(), [Event::CancelFailed]),
            "{events:?}"
        );
    }
}

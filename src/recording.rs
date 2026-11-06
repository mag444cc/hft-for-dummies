//! Recording the market, to replay it later (`backtest.rs`).
//!
//! `cargo run -- record <config> <file>` connects to the same feeds as the
//! bot would (Binance, Bybit if the config has it, and the exchange's book
//! and trades) and writes every event to `<file>`, one JSON line each, until
//! Ctrl-C. It never quotes or trades.
//!
//! The first line says which market it is and its price rules; every other
//! line is one event with the time we received it (µs since 1970). Prices
//! from Binance and Bybit arrive hundreds of times a second, more than the
//! bot can use, so we keep at most one per [`REFERENCE_EVERY`]; books, trades
//! and connection drops are all kept.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::book::Book;
use crate::config::{Config, VenueName};
use crate::events::{Event, FlowMarket, Stamp};
use crate::strategy::Ticks;
use crate::{binance, bullet, bybit, flow, hyperliquid};

/// Keep at most one Binance (and one Bybit) price per this long.
pub const REFERENCE_EVERY: Duration = Duration::from_millis(20);

/// Book levels kept per side: enough for the queue at our prices.
const BOOK_LEVELS: usize = 10;

/// One line of a recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Line {
    /// The first line: what was recorded.
    Header {
        venue: VenueName,
        symbol: String,
        ticks: Ticks,
    },
    Binance(Prices),
    Bybit(Prices),
    Book {
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        at_us: u64,
        exchange_us: Option<u64>,
    },
    Trade {
        price: Decimal,
        size: Decimal,
        at_us: u64,
        exchange_us: Option<u64>,
    },
    /// Aggressive buying and selling on a reference market (for CVD).
    Flow {
        market: FlowMarket,
        buy: Decimal,
        sell: Decimal,
        at_us: u64,
        exchange_us: Option<u64>,
    },
    /// Our copy of the book went out of sync.
    BookLost {
        at_us: u64,
    },
    /// A feed dropped and reconnected: things may have been missed.
    Reconnecting {
        at_us: u64,
    },
}

/// A best bid and ask from a reference exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prices {
    pub bid: Decimal,
    pub ask: Decimal,
    pub at_us: u64,
    pub exchange_us: Option<u64>,
    pub match_us: Option<u64>,
}

impl Line {
    /// The line for a market event, or `None` for events that aren't the
    /// market's (our own orders, timers).
    pub fn from_event(event: &Event) -> Option<Line> {
        Some(match event {
            Event::Reference { bid, ask, stamp } => Line::Binance(Prices::from(*bid, *ask, stamp)),
            Event::SecondReference { bid, ask, stamp } => {
                Line::Bybit(Prices::from(*bid, *ask, stamp))
            }
            Event::Book(book, stamp) => Line::Book {
                bids: book
                    .bids
                    .iter()
                    .rev()
                    .take(BOOK_LEVELS)
                    .map(|(p, s)| (*p, *s))
                    .collect(),
                asks: book
                    .asks
                    .iter()
                    .take(BOOK_LEVELS)
                    .map(|(p, s)| (*p, *s))
                    .collect(),
                at_us: stamp.received_unix_us,
                exchange_us: stamp.exchange_unix_us,
            },
            Event::Trade { price, size, stamp } => Line::Trade {
                price: *price,
                size: *size,
                at_us: stamp.received_unix_us,
                exchange_us: stamp.exchange_unix_us,
            },
            Event::Flow {
                market,
                buy,
                sell,
                stamp,
            } => Line::Flow {
                market: *market,
                buy: *buy,
                sell: *sell,
                at_us: stamp.received_unix_us,
                exchange_us: stamp.exchange_unix_us,
            },
            Event::BookLost => Line::BookLost { at_us: unix_us() },
            Event::Reconnecting => Line::Reconnecting { at_us: unix_us() },
            _ => return None,
        })
    }

    /// When we received it (µs since 1970); the header has none.
    pub fn at_us(&self) -> Option<u64> {
        match self {
            Line::Header { .. } => None,
            Line::Binance(p) | Line::Bybit(p) => Some(p.at_us),
            Line::Book { at_us, .. }
            | Line::Trade { at_us, .. }
            | Line::Flow { at_us, .. }
            | Line::BookLost { at_us }
            | Line::Reconnecting { at_us } => Some(*at_us),
        }
    }

    /// Back to the event the bot saw, at `at` on the replay's clock.
    pub fn to_event(&self, at: Instant) -> Option<Event> {
        let stamp = |at_us: u64, exchange_us: Option<u64>, match_us: Option<u64>| Stamp {
            received_at: at,
            received_unix_us: at_us,
            exchange_unix_us: exchange_us,
            exchange_match_unix_us: match_us,
        };
        Some(match self {
            Line::Header { .. } => return None,
            Line::Binance(p) => Event::Reference {
                bid: p.bid,
                ask: p.ask,
                stamp: stamp(p.at_us, p.exchange_us, p.match_us),
            },
            Line::Bybit(p) => Event::SecondReference {
                bid: p.bid,
                ask: p.ask,
                stamp: stamp(p.at_us, p.exchange_us, p.match_us),
            },
            Line::Book {
                bids,
                asks,
                at_us,
                exchange_us,
            } => Event::Book(
                Book {
                    bids: bids.iter().copied().collect(),
                    asks: asks.iter().copied().collect(),
                },
                stamp(*at_us, *exchange_us, None),
            ),
            Line::Trade {
                price,
                size,
                at_us,
                exchange_us,
            } => Event::Trade {
                price: *price,
                size: *size,
                stamp: stamp(*at_us, *exchange_us, *exchange_us),
            },
            Line::Flow {
                market,
                buy,
                sell,
                at_us,
                exchange_us,
            } => Event::Flow {
                market: *market,
                buy: *buy,
                sell: *sell,
                stamp: stamp(*at_us, *exchange_us, *exchange_us),
            },
            Line::BookLost { .. } => Event::BookLost,
            Line::Reconnecting { .. } => Event::Reconnecting,
        })
    }
}

impl Prices {
    fn from(bid: Decimal, ask: Decimal, stamp: &Stamp) -> Self {
        Self {
            bid,
            ask,
            at_us: stamp.received_unix_us,
            exchange_us: stamp.exchange_unix_us,
            match_us: stamp.exchange_match_unix_us,
        }
    }
}

/// Read a recording: its header, then its lines in order.
pub fn read(path: &Path) -> Result<(Line, impl Iterator<Item = Result<Line>>)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let first = lines.next().context("the recording is empty")??;
    let header: Line = serde_json::from_str(&first).context("reading the recording's header")?;
    if !matches!(header, Line::Header { .. }) {
        bail!(
            "{} doesn't start with a header: is it a recording?",
            path.display()
        );
    }
    let rest = lines.map(|line| Ok(serde_json::from_str::<Line>(&line?)?));
    Ok((header, rest))
}

/// `cargo run -- record`: record the market `config` quotes until Ctrl-C.
pub async fn record(config_path: &Path, out: &Path) -> Result<()> {
    let mut config = Config::load(config_path)?;
    config.dry_run = true; // recording needs no key and never trades
    if out.exists() {
        bail!(
            "{} already exists; not overwriting a recording",
            out.display()
        );
    }
    let (bus, mut events) = mpsc::unbounded_channel();
    let ticks = match config.venue {
        VenueName::Bullet => Ticks::Fixed(bullet::connect(&config, bus.clone()).await?.tick),
        VenueName::Hyperliquid => hyperliquid::connect(&config, bus.clone()).await?.ticks,
    };
    binance::connect(&config.binance_symbol, bus.clone());
    if let Some(symbol) = &config.bybit_symbol {
        bybit::connect(symbol, bus.clone());
    }
    if config.cvd_enabled() {
        flow::connect(
            &config.binance_symbol,
            config.cvd_spot_symbol.as_deref(),
            bus.clone(),
        );
    }
    {
        let bus = bus.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = bus.send(Event::Shutdown);
            }
        });
    }

    let mut file =
        BufWriter::new(File::create(out).with_context(|| format!("creating {}", out.display()))?);
    let header = Line::Header {
        venue: config.venue,
        symbol: config.symbol.clone(),
        ticks,
    };
    writeln!(file, "{}", serde_json::to_string(&header)?)?;
    tracing::info!(
        "recording {} to {} (Ctrl-C to stop)",
        config.symbol,
        out.display()
    );

    let mut thin = Thinning::default();
    let (mut written, mut last_report) = (0u64, Instant::now());
    while let Some(event) = events.recv().await {
        if matches!(event, Event::Shutdown) {
            break;
        }
        let Some(line) = Line::from_event(&event) else {
            continue;
        };
        if !thin.keep(&line) {
            continue;
        }
        writeln!(file, "{}", serde_json::to_string(&line)?)?;
        written += 1;
        if last_report.elapsed() >= Duration::from_secs(30) {
            file.flush()?;
            tracing::info!("{written} events recorded");
            last_report = Instant::now();
        }
    }
    file.flush()?;
    tracing::info!("stopped: {written} events in {}", out.display());
    Ok(())
}

/// Keeps at most one reference price per [`REFERENCE_EVERY`], per exchange.
#[derive(Default)]
struct Thinning {
    /// When we last kept one of each (`None`: not yet).
    binance_us: Option<u64>,
    bybit_us: Option<u64>,
}

impl Thinning {
    fn keep(&mut self, line: &Line) -> bool {
        let every = REFERENCE_EVERY.as_micros() as u64;
        let last = match line {
            Line::Binance(_) => &mut self.binance_us,
            Line::Bybit(_) => &mut self.bybit_us,
            _ => return true,
        };
        let at = line.at_us().unwrap_or(0);
        if last.is_some_and(|last| at < last + every) {
            return false;
        }
        *last = Some(at);
        true
    }
}

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn stamped(at_us: u64) -> Stamp {
        Stamp {
            received_at: Instant::now(),
            received_unix_us: at_us,
            exchange_unix_us: Some(at_us - 500),
            exchange_match_unix_us: Some(at_us - 700),
        }
    }

    /// Event -> line -> JSON -> line -> event gives back the same event.
    fn round_trip(event: Event) -> Event {
        let line = Line::from_event(&event).unwrap();
        let json = serde_json::to_string(&line).unwrap();
        let back: Line = serde_json::from_str(&json).unwrap();
        assert_eq!(back, line, "{json}");
        back.to_event(Instant::now()).unwrap()
    }

    #[test]
    fn reference_prices_round_trip() {
        let Event::Reference { bid, ask, stamp } = round_trip(Event::Reference {
            bid: d("90.1"),
            ask: d("90.2"),
            stamp: stamped(1_000_000),
        }) else {
            panic!()
        };
        assert_eq!((bid, ask), (d("90.1"), d("90.2")));
        assert_eq!(
            (
                stamp.received_unix_us,
                stamp.exchange_unix_us,
                stamp.exchange_match_unix_us
            ),
            (1_000_000, Some(999_500), Some(999_300))
        );
        assert!(matches!(
            round_trip(Event::SecondReference {
                bid: d("90.1"),
                ask: d("90.2"),
                stamp: stamped(1_000_000),
            }),
            Event::SecondReference { .. }
        ));
    }

    #[test]
    fn books_and_trades_round_trip() {
        let book = Book::from_levels(
            &[(d("90.1"), d("5")), (d("90.0"), d("2"))],
            &[(d("90.2"), d("3"))],
        );
        let Event::Book(back, stamp) = round_trip(Event::Book(book.clone(), stamped(2_000_000)))
        else {
            panic!()
        };
        assert_eq!(back, book);
        assert_eq!(stamp.received_unix_us, 2_000_000);
        let Event::Trade { price, size, .. } = round_trip(Event::Trade {
            price: d("90.15"),
            size: d("0.5"),
            stamp: stamped(3_000_000),
        }) else {
            panic!()
        };
        assert_eq!((price, size), (d("90.15"), d("0.5")));
    }

    #[test]
    fn our_own_orders_are_not_recorded() {
        assert!(Line::from_event(&Event::Tick).is_none());
        assert!(Line::from_event(&Event::OrderGone { client_id: 1 }).is_none());
    }

    #[test]
    fn keeps_one_reference_price_per_20_ms_but_every_book() {
        let mut thin = Thinning::default();
        let price = |at_us| {
            Line::Binance(Prices {
                bid: d("1"),
                ask: d("2"),
                at_us,
                exchange_us: None,
                match_us: None,
            })
        };
        let kept: Vec<bool> = [0u64, 5_000, 19_999, 20_000, 30_000, 41_000]
            .iter()
            .map(|&at| thin.keep(&price(at)))
            .collect();
        assert_eq!(kept, vec![true, false, false, true, false, true]);
        assert!(
            thin.keep(&Line::BookLost { at_us: 41_001 }),
            "everything else is kept"
        );
    }

    #[test]
    fn the_header_round_trips() {
        let header = Line::Header {
            venue: VenueName::Hyperliquid,
            symbol: "SOL".into(),
            ticks: Ticks::SignificantFigures {
                figures: 5,
                max_decimals: 4,
            },
        };
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(serde_json::from_str::<Line>(&json).unwrap(), header);
    }
}

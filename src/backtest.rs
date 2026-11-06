//! Replaying a recording through the bot, to compare settings in seconds.
//!
//! `cargo run -- backtest <recording> <config or folder>...` feeds a
//! recording (`recording.rs`) to one market maker and paper executor per
//! config: the same code as a dry run, on the recording's clock instead of
//! the wall clock. Every config sees exactly the same market, so the
//! comparison is fair, and an hour of market replays in seconds.
//!
//! Like a dry run it is kind to us in one way: our orders never move the
//! market. By default they also arrive and leave instantly; `--latency-ms`
//! makes every order, change and cancel take that long to reach the
//! exchange, as it would in real life (a quote that's being cancelled can
//! still be hit in the meantime).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::events::{Event, Intent};
use crate::executor::{Executor, PaperExecutor};
use crate::market_maker::{MarketMaker, Results};
use crate::recording::{self, Line};
use crate::strategy::Ticks;

/// One config being replayed.
struct Run {
    name: String,
    market_maker: MarketMaker,
    paper: PaperExecutor,
    /// The paper executor's fills come back here, as they would on the bus.
    fills: mpsc::UnboundedReceiver<Event>,
    /// Orders placed, moved or cancelled: what would count against a rate limit.
    requests: u64,
    /// How long our requests take to reach the exchange.
    latency: Duration,
    /// Requests on their way, and when they arrive.
    in_flight: VecDeque<(Instant, Vec<Intent>)>,
}

/// How one config did.
#[derive(Debug, Clone)]
pub struct Report {
    pub name: String,
    pub results: Results,
    pub requests: u64,
}

impl Run {
    fn new(name: String, config: Config, ticks: Ticks, latency: Duration) -> Self {
        let (bus, fills) = mpsc::unbounded_channel();
        let fee = config.paper_fee_bps.unwrap_or_default();
        let mut run = Self {
            name,
            market_maker: MarketMaker::new(config, ticks),
            paper: PaperExecutor::new(bus, fee),
            fills,
            requests: 0,
            latency,
            in_flight: VecDeque::new(),
        };
        // Startup (clearing old orders) happens before the replay starts.
        let start = run.market_maker.start();
        run.count(&start);
        run.paper.execute(start);
        run
    }

    /// Send `intents` at `now`: they reach the exchange `latency` later.
    fn execute(&mut self, intents: Vec<Intent>, now: Instant) {
        self.count(&intents);
        if self.latency.is_zero() {
            self.paper.execute(intents);
        } else if !intents.is_empty() {
            self.in_flight.push_back((now + self.latency, intents));
        }
    }

    /// Requests that have reached the exchange by `now` take effect.
    fn arrive(&mut self, now: Instant) {
        while self.in_flight.front().is_some_and(|(at, _)| *at <= now) {
            if let Some((_, intents)) = self.in_flight.pop_front() {
                self.paper.execute(intents);
            }
        }
    }

    fn count(&mut self, intents: &[Intent]) {
        self.requests += intents
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    Intent::Place(_) | Intent::Replace { .. } | Intent::Cancel { .. }
                )
            })
            .count() as u64;
    }

    /// The same steps as the bot's loop, for one event at `now`.
    fn on_event(&mut self, event: &Event, now: Instant) {
        self.arrive(now);
        self.paper.on_event(event);
        let intents = self.market_maker.on_event(event, now);
        self.execute(intents, now);
        // Fills the paper executor just made, before the next event.
        while let Ok(fill) = self.fills.try_recv() {
            let intents = self.market_maker.on_event(&fill, now);
            self.execute(intents, now);
        }
        let intents = self.market_maker.requote(now);
        self.execute(intents, now);
    }
}

/// Replay `lines` through each config. Returns how each did, and how long
/// the recording covered.
pub fn replay(
    ticks: Ticks,
    lines: impl Iterator<Item = Line>,
    configs: Vec<(String, Config)>,
    latency: Duration,
) -> (Vec<Report>, Duration) {
    let mut runs: Vec<Run> = configs
        .into_iter()
        .map(|(name, config)| Run::new(name, config, ticks, latency))
        .collect();
    // The replay's clock: the first event is "now", the rest follow at their
    // recorded distance from it.
    let base = Instant::now();
    let (mut first_us, mut last_us, mut next_tick_us) = (None::<u64>, 0u64, 0u64);
    for line in lines {
        let Some(at_us) = line.at_us() else { continue };
        let first = *first_us.get_or_insert(at_us);
        if next_tick_us == 0 {
            next_tick_us = at_us + 1_000_000;
        }
        // Feeds interleave; never let the clock run backwards.
        let at_us = at_us.max(last_us);
        last_us = at_us;
        // The bot's once-a-second timer, for every second that has passed.
        while next_tick_us <= at_us {
            let tick_at = base + Duration::from_micros(next_tick_us - first);
            runs.iter_mut()
                .for_each(|run| run.on_event(&Event::Tick, tick_at));
            next_tick_us += 1_000_000;
        }
        let now = base + Duration::from_micros(at_us - first);
        if let Some(event) = line.to_event(now) {
            runs.iter_mut().for_each(|run| run.on_event(&event, now));
        }
    }
    let covered = Duration::from_micros(last_us - first_us.unwrap_or(last_us));
    let reports = runs
        .into_iter()
        .map(|run| Report {
            results: run.market_maker.results(),
            name: run.name,
            requests: run.requests,
        })
        .collect();
    (reports, covered)
}

/// `cargo run -- backtest`: replay a recording through every config given
/// (files, or folders of `.toml` files) and print the comparison.
pub fn backtest(recording_path: &Path, config_paths: &[PathBuf], latency: Duration) -> Result<()> {
    let (header, lines) = recording::read(recording_path)?;
    let Line::Header { symbol, ticks, .. } = &header else {
        bail!("no header")
    };
    let mut configs = Vec::new();
    for path in config_paths {
        let files: Vec<PathBuf> = if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
                .map(|entry| entry.map(|e| e.path()))
                .collect::<std::io::Result<_>>()?;
            files.retain(|p| p.extension().is_some_and(|e| e == "toml"));
            files.sort();
            files
        } else {
            vec![path.clone()]
        };
        for file in files {
            let mut config = Config::load(&file)?;
            config.dry_run = true;
            if &config.symbol != symbol {
                bail!(
                    "{} quotes {}, but the recording is {symbol}",
                    file.display(),
                    config.symbol
                );
            }
            let name = file.file_stem().map_or_else(
                || file.display().to_string(),
                |s| s.to_string_lossy().into(),
            );
            configs.push((name, config));
        }
    }
    if configs.is_empty() {
        bail!("no configs to replay");
    }
    let started = Instant::now();
    let lines = lines
        .collect::<Result<Vec<Line>>>()
        .context("reading the recording")?;
    let count = lines.len();
    let (mut reports, covered) = replay(*ticks, lines.into_iter(), configs, latency);
    println!(
        "{symbol}: {count} events covering {:.1} hours, replayed through {} configs in {:.1} s\n",
        covered.as_secs_f64() / 3600.0,
        reports.len(),
        started.elapsed().as_secs_f64()
    );
    reports.sort_by_key(|r| std::cmp::Reverse(r.results.after_fees.unwrap_or(Decimal::MIN)));
    print!("{}", table(&reports, covered));
    Ok(())
}

/// The comparison, best first: profit split into spread, position and fees.
fn table(reports: &[Report], covered: Duration) -> String {
    let minutes = (covered.as_secs_f64() / 60.0).max(1.0 / 60.0);
    let mut out = format!(
        "{:<24} {:>6} {:>9} {:>9} {:>8} {:>10} {:>8}  {}\n",
        "config", "trades", "spread", "position", "fees", "AFTER FEES", "req/min", "stopped?"
    );
    for r in reports {
        let res = &r.results;
        let after = res.after_fees.unwrap_or(Decimal::ZERO);
        // after fees = spread + position - fees, so position is what's left.
        let position = after + res.fees - res.spread_profit;
        out += &format!(
            "{:<24} {:>6} {:>+9.3} {:>+9.3} {:>8.3} {:>+10.3} {:>8.1}  {}\n",
            r.name,
            res.trades,
            res.spread_profit,
            position,
            res.fees,
            after,
            r.requests as f64 / minutes,
            if res.stopped { "loss limit" } else { "" }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::recording::Prices;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// Quote 1 unit `spread` bps either side of fair value, requote freely.
    fn config(spread: &str) -> Config {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        config.spread_bps = d(spread);
        config.skew_bps = Decimal::ZERO;
        config.order_size = d("1");
        config.max_position = d("2");
        config.min_requote_ms = 0;
        config.paper_fee_bps = Some(d("1.5"));
        config
    }

    /// 70 s of a quiet market around 100 (Binance and the book, 10 a second,
    /// long enough to warm up), then someone sells 5 at 99.80.
    fn market() -> Vec<Line> {
        let start = 1_700_000_000_000_000u64;
        let mut lines = Vec::new();
        for step in 0..700u64 {
            let at_us = start + step * 100_000;
            lines.push(Line::Binance(Prices {
                bid: d("99.5"),
                ask: d("100.5"),
                at_us,
                exchange_us: None,
                match_us: None,
            }));
            lines.push(Line::Book {
                bids: vec![(d("99.5"), d("10"))],
                asks: vec![(d("100.5"), d("10"))],
                at_us,
                exchange_us: None,
            });
        }
        lines.push(Line::Trade {
            price: d("99.80"),
            size: d("5"),
            at_us: start + 70_000_000,
            exchange_us: Some(start + 70_000_000),
        });
        lines
    }

    #[test]
    fn a_trade_through_our_bid_fills_it() {
        // 10 bps: our bid is 99.90, and the sale at 99.80 went through it.
        let (reports, covered) = replay(
            Ticks::Fixed(d("0.01")),
            market().into_iter(),
            vec![("ten".into(), config("10"))],
            Duration::ZERO,
        );
        let r = &reports[0].results;
        assert_eq!((r.trades, r.position), (1, d("1")));
        assert_eq!(
            r.spread_profit,
            d("0.10"),
            "bought 1 at 99.90, fair value 100"
        );
        assert_eq!(r.fees, d("0.014985"), "1.5 bps of 99.90");
        assert!(reports[0].requests >= 2, "placed a bid and an ask");
        assert_eq!(covered, Duration::from_secs(70));
    }

    #[test]
    fn every_config_sees_the_same_market_but_trades_its_own_way() {
        // 30 bps: the bid is 99.70, below the sale at 99.80: no fill.
        let (reports, _) = replay(
            Ticks::Fixed(d("0.01")),
            market().into_iter(),
            vec![
                ("ten".into(), config("10")),
                ("thirty".into(), config("30")),
            ],
            Duration::ZERO,
        );
        assert_eq!(reports[0].results.trades, 1);
        assert_eq!(reports[1].results.trades, 0);
    }

    #[test]
    fn a_slow_order_can_miss_the_trade() {
        // We first quote once warmed up, about 60 s in; the sale at 99.80 is
        // at 70 s. Instantly (or nearly) our bid is there for it; with a 15 s
        // delay it only lands at about 75 s, after the sale.
        let (fast, _) = replay(
            Ticks::Fixed(d("0.01")),
            market().into_iter(),
            vec![("ten".into(), config("10"))],
            Duration::from_millis(1),
        );
        assert_eq!(fast[0].results.trades, 1, "a 1 ms delay changes nothing");
        let (slow, _) = replay(
            Ticks::Fixed(d("0.01")),
            market().into_iter(),
            vec![("ten".into(), config("10"))],
            Duration::from_secs(15),
        );
        assert_eq!(slow[0].results.trades, 0, "the bid wasn't there yet");
    }

    #[test]
    fn the_table_splits_profit() {
        let (reports, covered) = replay(
            Ticks::Fixed(d("0.01")),
            market().into_iter(),
            vec![("ten".into(), config("10"))],
            Duration::ZERO,
        );
        let table = table(&reports, covered);
        assert!(table.contains("ten"), "{table}");
        assert!(table.contains("+0.100"), "spread profit column: {table}");
    }
}

//! The harness: wires the pieces together and runs the loop.
//!
//! ```text
//!  Binance, Bybit ┐                                  ┌── LiveExecutor ──> Bullet or Hyperliquid
//!  exchange feed ─┼──> bus ──> MarketMaker ──> intents
//!  timer, Ctrl-C ─┘     ^       (state machine)      └── PaperExecutor (dry run)
//!                       └──────── fills, acks, failures ──────┘
//! ```
//!
//! Every input becomes an event on one queue. The loop takes them one at a
//! time, lets the market maker update its view, and once it has caught up
//! with everything waiting, asks it what orders it wants.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::{Config, VenueName};
use crate::events::{Bus, Event, Intent, Stamp};
use crate::executor::{Executor, LiveExecutor, PaperExecutor, Venue};
use crate::market_maker::MarketMaker;
use crate::strategy::Ticks;
use crate::telemetry::{self, Telemetry};
use crate::{binance, bullet, bybit, flow, history, hyperliquid};

pub async fn run(config_path: &Path, live: bool) -> Result<()> {
    let config = Config::load(config_path)?.live(live);
    let venue = format!("{:?}", config.venue).to_lowercase();
    if config.dry_run {
        tracing::info!(
            "DRY RUN: watching {} on {venue} and logging the quotes we would place. No orders are sent.",
            config.symbol
        );
    } else {
        tracing::warn!("LIVE: trading {} on {venue} with real money", config.symbol);
    }

    // Metrics are a window, not a dependency: if the port is taken, run without.
    if let Some(port) = config.metrics_port
        && let Err(e) = telemetry::serve_metrics(port)
    {
        tracing::warn!("no metrics for Grafana (port {port}): {e}");
    }

    let (bus, events) = mpsc::unbounded_channel();
    // The exchange first: it takes a second to connect, and Binance ticks
    // queued meanwhile would show up as fake queueing delay.
    match config.venue {
        VenueName::Bullet => {
            let bullet = bullet::connect(&config, bus.clone()).await?;
            let ticks = Ticks::Fixed(bullet.tick);
            trade(config, bullet, ticks, bus, events).await
        }
        VenueName::Hyperliquid => {
            let hyperliquid = hyperliquid::connect(&config, bus.clone()).await?;
            let ticks = hyperliquid.ticks;
            trade(config, hyperliquid, ticks, bus, events).await
        }
    }
}

/// Everything after connecting is the same on every exchange.
async fn trade(
    config: Config,
    venue: impl Venue,
    ticks: Ticks,
    bus: Bus,
    events: mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
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
    if history::needed(&config) {
        history::fetch(&config.binance_symbol, bus.clone());
    }
    spawn_timer(bus.clone());
    spawn_ctrl_c(bus.clone());

    let dry_run = config.dry_run;
    let paper_fee = config.paper_fee_bps.unwrap_or_default();
    let market_maker = MarketMaker::new(config, ticks);
    if dry_run {
        event_loop(market_maker, PaperExecutor::new(bus, paper_fee), events).await
    } else {
        event_loop(market_maker, LiveExecutor::new(venue, bus), events).await
    }
}

async fn event_loop(
    mut market_maker: MarketMaker,
    mut executor: impl Executor,
    mut events: mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
    let mut telemetry = Telemetry::new();

    executor.execute(market_maker.start());
    let result = loop {
        let Some(event) = events.recv().await else {
            break Ok(());
        };
        match &event {
            Event::Shutdown => break Ok(()),
            Event::Disconnected(why) => break Err(anyhow::anyhow!("Bullet disconnected: {why}")),
            Event::Tick => telemetry.maybe_report(Instant::now()),
            _ => {}
        }
        let now = Instant::now();
        // A Binance update is the "tick" we time our reaction and the whole
        // trip from. A requote caused by anything else (a fill, Bullet's book)
        // has no Binance update behind it, so it isn't timed.
        let tick: Option<Stamp> = match &event {
            Event::Reference { stamp, .. } => Some(*stamp),
            _ => None,
        };
        telemetry.on_event(&event, now);
        executor.on_event(&event);
        let intents = market_maker.on_event(&event, now);
        let mut decide = now.elapsed();
        executor.execute(intents);

        // Only quote on the newest information: if more events are already
        // waiting, read them first. A quote based on a price we know is old
        // is a gift to whoever trades against it.
        if events.is_empty() {
            let started = Instant::now();
            let intents = market_maker.requote(started);
            let decided = Instant::now();
            decide += decided - started;
            telemetry.record(telemetry::DECIDE, decide);
            if tick.is_some()
                && intents
                    .iter()
                    .any(|i| matches!(i, Intent::Place(_) | Intent::Replace { .. }))
            {
                // From picking the update up, so time waiting in the queue
                // (step 3) isn't counted twice.
                telemetry.record(telemetry::TICK_TO_TRADE, decided - now);
            }
            telemetry.on_decision(&intents, tick.as_ref(), now, decided);
            executor.execute(intents);
        }
    };
    tracing::info!("shutting down: cancelling our orders");
    executor.execute(market_maker.stop());
    executor.finish().await; // wait for the final cancel to go through

    // An order can fill in the instant before our cancel lands. Keep listening
    // for a moment so the final numbers include those fills.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let Event::Fill(_) = event {
            market_maker.on_event(&event, Instant::now());
        }
    }
    market_maker.log_summary(Instant::now());
    tracing::info!("latency this run:\n{}", telemetry.table());
    tracing::info!("{}", telemetry.scoreboard());
    result
}

fn spawn_timer(bus: Bus) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        loop {
            timer.tick().await;
            if bus.send(Event::Tick).is_err() {
                return;
            }
        }
    });
}

fn spawn_ctrl_c(bus: Bus) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = bus.send(Event::Shutdown);
        }
    });
}

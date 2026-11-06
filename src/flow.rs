//! Order flow: who is trading aggressively, buyers or sellers?
//!
//! Every trade has an aggressor: someone who crossed the spread to buy or
//! sell at once. Adding up aggressive buying minus aggressive selling gives
//! the *cumulative volume delta* (CVD), the line charting sites draw. We
//! build it ourselves from two free trade feeds:
//!
//! - Binance USDT perps (`aggTrade`): the busiest futures market;
//! - Bybit spot (`publicTrade`): real coins changing hands, no leverage.
//!
//! Each trade goes to the bot as an [`Event::Flow`]; the market maker keeps
//! per-second totals (see [`Cvd`]) and can lean its quotes with them
//! (`cvd_short_lean_bps`, `cvd_long_lean_bps`, `cvd_divergence`).

use std::collections::VecDeque;
use std::str::FromStr;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::events::{Bus, Event, FlowMarket, Stamp};
use crate::liveness::{Liveness, SILENT_AFTER};

/// Trades come many times a second on these markets; this long without one
/// means the feed is broken.
const NO_TRADES_AFTER: Duration = Duration::from_secs(30);

/// Start Binance perp trades for `symbol` (e.g. "SOLUSDT") and, if given,
/// Bybit spot trades for `spot_symbol`, each on its own thread.
pub fn connect(symbol: &str, spot_symbol: Option<&str>, bus: Bus) {
    let perp = Feed {
        name: "binance-trades",
        url: format!(
            "wss://fstream.binance.com/market/ws/{}@aggTrade",
            symbol.to_lowercase()
        ),
        subscribe: None,
        ping: None,
        parse: parse_binance,
    };
    spawn(perp, bus.clone());
    if let Some(spot) = spot_symbol {
        let spot = Feed {
            name: "bybit-spot-trades",
            url: "wss://stream.bybit.com/v5/public/spot".into(),
            subscribe: Some(format!(
                r#"{{"op":"subscribe","args":["publicTrade.{spot}"]}}"#
            )),
            ping: Some(r#"{"op":"ping"}"#),
            parse: parse_bybit_spot,
        };
        spawn(spot, bus);
    }
}

struct Feed {
    name: &'static str,
    url: String,
    subscribe: Option<String>,
    /// A text ping to send every second (Bybit), or websocket pings (Binance).
    ping: Option<&'static str>,
    parse: fn(&str) -> Vec<Event>,
}

fn spawn(feed: Feed, bus: Bus) {
    std::thread::Builder::new()
        .name(feed.name.into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building the feed's runtime");
            runtime.block_on(run(feed, bus));
        })
        .expect("starting the feed thread");
}

async fn run(feed: Feed, bus: Bus) {
    let name = feed.name;
    let mut backoff = Duration::from_secs(1);
    loop {
        match tokio::time::timeout(SILENT_AFTER, connect_async(&feed.url)).await {
            Ok(Ok((mut ws, _))) => {
                tracing::info!("{name}: connected");
                backoff = Duration::from_secs(1);
                if let Some(sub) = &feed.subscribe {
                    let _ = ws.send(Message::Text(sub.clone().into())).await;
                }
                let mut ping = tokio::time::interval(Duration::from_secs(1));
                let mut live = Liveness::new(Instant::now(), Some(NO_TRADES_AFTER));
                loop {
                    let msg = tokio::select! {
                        msg = ws.next() => match msg {
                            Some(msg) => msg,
                            None => break,
                        },
                        _ = ping.tick() => {
                            if let Some(why) = live.dead(Instant::now()) {
                                tracing::warn!("{name}: {why}");
                                break;
                            }
                            let sent = match feed.ping {
                                Some(text) => ws.send(Message::Text(text.to_string().into())).await,
                                None => ws.send(Message::Ping(Vec::new().into())).await,
                            };
                            if sent.is_err() {
                                break;
                            }
                            continue;
                        }
                    };
                    match msg {
                        Ok(Message::Text(text)) => {
                            let events = (feed.parse)(&text);
                            live.heard(Instant::now(), !events.is_empty());
                            for event in events {
                                if bus.send(event).is_err() {
                                    return; // the bot has shut down
                                }
                            }
                        }
                        Ok(Message::Ping(payload)) => {
                            live.heard(Instant::now(), false);
                            if ws.send(Message::Pong(payload)).await.is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => live.heard(Instant::now(), false),
                    }
                }
                tracing::warn!("{name}: disconnected, reconnecting");
            }
            Ok(Err(e)) => tracing::warn!("{name}: connect failed: {e}"),
            Err(_) => tracing::warn!("{name}: connect timed out"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

#[derive(Deserialize)]
struct BinanceTrade {
    #[serde(rename = "q")]
    qty: String,
    /// True when the buyer was the resting order, i.e. a seller was aggressive.
    #[serde(rename = "m")]
    buyer_is_maker: bool,
    #[serde(rename = "T")]
    trade_ms: u64,
}

fn parse_binance(text: &str) -> Vec<Event> {
    let Ok(t) = serde_json::from_str::<BinanceTrade>(text) else {
        return Vec::new();
    };
    let Ok(qty) = Decimal::from_str(&t.qty) else {
        return Vec::new();
    };
    vec![flow(FlowMarket::Perp, !t.buyer_is_maker, qty, t.trade_ms)]
}

#[derive(Deserialize)]
struct BybitTrades {
    topic: String,
    data: Vec<BybitTrade>,
}

#[derive(Deserialize)]
struct BybitTrade {
    /// The aggressor's side: "Buy" or "Sell".
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "v")]
    qty: String,
    #[serde(rename = "T")]
    trade_ms: u64,
}

fn parse_bybit_spot(text: &str) -> Vec<Event> {
    let Ok(msg) = serde_json::from_str::<BybitTrades>(text) else {
        return Vec::new(); // pongs, subscription answers
    };
    if !msg.topic.starts_with("publicTrade.") {
        return Vec::new();
    }
    msg.data
        .iter()
        .filter_map(|t| {
            let qty = Decimal::from_str(&t.qty).ok()?;
            Some(flow(FlowMarket::Spot, t.side == "Buy", qty, t.trade_ms))
        })
        .collect()
}

fn flow(market: FlowMarket, aggressive_buy: bool, qty: Decimal, trade_ms: u64) -> Event {
    let (buy, sell) = if aggressive_buy {
        (qty, Decimal::ZERO)
    } else {
        (Decimal::ZERO, qty)
    };
    Event::Flow {
        market,
        buy,
        sell,
        stamp: Stamp::now(Some(trade_ms * 1000), Some(trade_ms * 1000)),
    }
}

/// Aggressive buying and selling per second, for as long as the longest
/// window needs, per market.
pub struct Cvd {
    /// (second it started, perp buy, perp sell, spot buy, spot sell).
    seconds: VecDeque<(Instant, [f64; 4])>,
    keep: Duration,
}

impl Cvd {
    pub fn new(keep: Duration) -> Self {
        Self {
            seconds: VecDeque::new(),
            keep,
        }
    }

    pub fn on_flow(&mut self, market: FlowMarket, buy: f64, sell: f64, at: Instant) {
        let fresh = self.seconds.back().is_none_or(|(start, _)| {
            at.saturating_duration_since(*start) >= Duration::from_secs(1)
        });
        if fresh {
            self.seconds.push_back((at, [0.0; 4]));
        }
        if let Some((_, totals)) = self.seconds.back_mut() {
            let i = match market {
                FlowMarket::Perp => 0,
                FlowMarket::Spot => 2,
            };
            totals[i] += buy;
            totals[i + 1] += sell;
        }
        while self
            .seconds
            .front()
            .is_some_and(|(start, _)| at.saturating_duration_since(*start) > self.keep)
        {
            self.seconds.pop_front();
        }
    }

    /// Who was more aggressive over the last `window`, on `market`: from -1
    /// (only selling) to +1 (only buying). `None` without any trades.
    pub fn imbalance(&self, market: FlowMarket, window: Duration, now: Instant) -> Option<f64> {
        let i = match market {
            FlowMarket::Perp => 0,
            FlowMarket::Spot => 2,
        };
        let (mut buy, mut sell) = (0.0, 0.0);
        for (start, totals) in self.seconds.iter().rev() {
            if now.saturating_duration_since(*start) > window {
                break;
            }
            buy += totals[i];
            sell += totals[i + 1];
        }
        let total = buy + sell;
        (total > 0.0).then(|| (buy - sell) / total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_real_binance_agg_trade() {
        let msg = r#"{"e":"aggTrade","E":1793988041656,"a":1111564895,"s":"SOLUSDT","p":"89.9100","q":"0.06","nq":"0.06","f":1,"l":1,"T":1793988041600,"m":true}"#;
        let events = parse_binance(msg);
        let [
            Event::Flow {
                market, buy, sell, ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}")
        };
        // The buyer was the maker: an aggressive sell.
        assert_eq!(
            (*market, *buy, *sell),
            (FlowMarket::Perp, Decimal::ZERO, Decimal::new(6, 2))
        );
    }

    #[test]
    fn reads_bybit_spot_trades_and_ignores_pongs() {
        let msg = r#"{"topic":"publicTrade.SOLUSDT","type":"snapshot","ts":1,"data":[{"T":1,"s":"SOLUSDT","S":"Buy","v":"2.5","p":"90.1"},{"T":1,"s":"SOLUSDT","S":"Sell","v":"1","p":"90.0"}]}"#;
        let events = parse_bybit_spot(msg);
        assert_eq!(events.len(), 2);
        let Event::Flow { market, buy, .. } = &events[0] else {
            panic!()
        };
        assert_eq!((*market, *buy), (FlowMarket::Spot, Decimal::new(25, 1)));
        assert!(parse_bybit_spot(r#"{"success":true,"ret_msg":"pong","op":"ping"}"#).is_empty());
    }

    #[test]
    fn imbalance_over_a_window() {
        let mut cvd = Cvd::new(Duration::from_secs(900));
        let t = Instant::now();
        // Ten minutes ago: heavy selling. Last minute: mostly buying.
        cvd.on_flow(FlowMarket::Perp, 0.0, 10.0, t);
        let now = t + Duration::from_secs(600);
        cvd.on_flow(FlowMarket::Perp, 3.0, 1.0, now - Duration::from_secs(30));
        cvd.on_flow(FlowMarket::Perp, 1.0, 0.0, now);
        let minute = cvd
            .imbalance(FlowMarket::Perp, Duration::from_secs(60), now)
            .unwrap();
        assert!((minute - 0.6).abs() < 1e-9, "(4 - 1) / 5: {minute}");
        let all = cvd
            .imbalance(FlowMarket::Perp, Duration::from_secs(900), now)
            .unwrap();
        assert!((all - (4.0 - 11.0) / 15.0).abs() < 1e-9, "{all}");
        assert_eq!(
            cvd.imbalance(FlowMarket::Spot, Duration::from_secs(60), now),
            None
        );
    }

    #[test]
    fn forgets_what_is_older_than_it_keeps() {
        let mut cvd = Cvd::new(Duration::from_secs(60));
        let t = Instant::now();
        cvd.on_flow(FlowMarket::Spot, 5.0, 0.0, t);
        cvd.on_flow(FlowMarket::Spot, 0.0, 1.0, t + Duration::from_secs(120));
        let all = cvd.imbalance(
            FlowMarket::Spot,
            Duration::from_secs(3600),
            t + Duration::from_secs(120),
        );
        assert_eq!(all, Some(-1.0));
    }
}

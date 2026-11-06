//! Bybit: a second place to borrow our price from.
//!
//! Binance moves first most of the time, but Bybit's USDT perps are big too.
//! Blending in its price makes fair value a little steadier, and if Binance's
//! feed stalls the bot can keep quoting on Bybit's (see `fair_value.rs`).
//!
//! We listen to Bybit's level-1 order book (`orderbook.1.SOLUSDT`), which
//! sends the best bid and ask whenever either changes, and forward them as
//! [`Event::SecondReference`]. Built like `binance.rs`: its own thread, and
//! it reconnects when the connection, or the prices, go quiet.

use std::str::FromStr;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::events::{Bus, Event, Stamp};
use crate::liveness::{Liveness, SILENT_AFTER};

/// Bybit sends the best bid and ask many times a second (and a fresh
/// snapshot at least every 3 s even when nothing changes), so this long
/// without one means the feed is broken: reconnect.
const NO_PRICES_AFTER: Duration = Duration::from_secs(5);

/// Start sending `symbol`'s (e.g. "SOLUSDT") best bid/ask from Bybit's USDT
/// perps to `bus`. Runs forever, reconnecting if the connection drops.
pub fn connect(symbol: &str, bus: Bus) {
    let symbol = symbol.to_string();
    std::thread::Builder::new()
        .name("bybit-feed".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building the feed's runtime");
            runtime.block_on(run(symbol, bus));
        })
        .expect("starting the feed thread");
}

async fn run(symbol: String, bus: Bus) {
    let url = "wss://stream.bybit.com/v5/public/linear";
    let subscribe = format!(r#"{{"op":"subscribe","args":["orderbook.1.{symbol}"]}}"#);
    let mut backoff = Duration::from_secs(1);
    loop {
        match tokio::time::timeout(SILENT_AFTER, connect_async(url)).await {
            Ok(Ok((mut ws, _))) => {
                tracing::info!("bybit: connected");
                backoff = Duration::from_secs(1);
                let _ = ws.send(Message::Text(subscribe.clone().into())).await;
                let mut best = Best::default();
                let mut ping = tokio::time::interval(Duration::from_secs(1));
                let mut live = Liveness::new(Instant::now(), Some(NO_PRICES_AFTER));
                loop {
                    let msg = tokio::select! {
                        msg = ws.next() => match msg {
                            Some(msg) => msg,
                            None => break,
                        },
                        _ = ping.tick() => {
                            if let Some(why) = live.dead(Instant::now()) {
                                tracing::warn!("bybit: {why}");
                                break;
                            }
                            let ping = r#"{"op":"ping"}"#.to_string();
                            if ws.send(Message::Text(ping.into())).await.is_err() {
                                break;
                            }
                            continue;
                        }
                    };
                    match msg {
                        Ok(Message::Text(text)) => {
                            let event = best.on_message(&text, &symbol);
                            // Only a price counts as data; pongs just show we're connected.
                            live.heard(Instant::now(), event.is_some());
                            if let Some(event) = event
                                && bus.send(event).is_err()
                            {
                                return; // the bot has shut down
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
                tracing::warn!("bybit: disconnected, reconnecting");
            }
            Ok(Err(e)) => tracing::warn!("bybit: connect failed: {e}"),
            Err(_) => tracing::warn!("bybit: connect timed out"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// The best bid and ask so far. A snapshot replaces them; a delta changes
/// only the sides it mentions (size 0 means that side is gone for now).
#[derive(Default)]
struct Best {
    bid: Option<Decimal>,
    ask: Option<Decimal>,
}

#[derive(Deserialize)]
struct BookMessage {
    topic: String,
    #[serde(rename = "type")]
    kind: String,
    /// When Bybit sent it (ms since 1970).
    ts: u64,
    /// When the book changed in the matching engine (ms since 1970).
    #[serde(default)]
    cts: Option<u64>,
    data: BookData,
}

#[derive(Deserialize)]
struct BookData {
    #[serde(default)]
    b: Vec<[String; 2]>,
    #[serde(default)]
    a: Vec<[String; 2]>,
}

impl Best {
    /// What one message means: new best prices for the bot, or nothing
    /// (pongs, subscription answers, other symbols, a side missing).
    fn on_message(&mut self, text: &str, symbol: &str) -> Option<Event> {
        let msg: BookMessage = serde_json::from_str(text).ok()?;
        if msg.topic != format!("orderbook.1.{symbol}") {
            return None;
        }
        let level = |levels: &[[String; 2]]| -> Option<Option<Decimal>> {
            let [price, size] = levels.first()?;
            let size = Decimal::from_str(size).ok()?;
            Some(if size.is_zero() {
                None
            } else {
                Decimal::from_str(price).ok()
            })
        };
        let (bid, ask) = (level(&msg.data.b), level(&msg.data.a));
        if msg.kind == "snapshot" {
            self.bid = bid.flatten();
            self.ask = ask.flatten();
        } else {
            if let Some(bid) = bid {
                self.bid = bid;
            }
            if let Some(ask) = ask {
                self.ask = ask;
            }
        }
        let (bid, ask) = (self.bid?, self.ask?);
        Some(Event::SecondReference {
            bid,
            ask,
            stamp: Stamp::now(Some(msg.ts * 1000), msg.cts.map(|ms| ms * 1000)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn prices(event: Option<Event>) -> Option<(Decimal, Decimal)> {
        match event? {
            Event::SecondReference { bid, ask, .. } => Some((bid, ask)),
            _ => None,
        }
    }

    #[test]
    fn reads_a_real_snapshot() {
        let mut best = Best::default();
        let msg = r#"{"topic":"orderbook.1.SOLUSDT","ts":1793964243469,"type":"snapshot","data":{"s":"SOLUSDT","b":[["90.28","394.5"]],"a":[["90.29","1423.1"]],"u":121236277,"seq":288326213913},"cts":1793964243465}"#;
        let event = best.on_message(msg, "SOLUSDT");
        let Some(Event::SecondReference { stamp, .. }) = &event else {
            panic!("{event:?}")
        };
        assert_eq!(stamp.exchange_unix_us, Some(1_793_964_243_469_000));
        assert_eq!(prices(event), Some((d("90.28"), d("90.29"))));
    }

    #[test]
    fn a_delta_changes_only_the_sides_it_names() {
        let mut best = Best::default();
        best.on_message(r#"{"topic":"orderbook.1.SOLUSDT","ts":1,"type":"snapshot","data":{"b":[["90.28","1"]],"a":[["90.29","1"]]}}"#, "SOLUSDT");
        let event = best.on_message(r#"{"topic":"orderbook.1.SOLUSDT","ts":2,"type":"delta","data":{"b":[],"a":[["90.30","5"]]}}"#, "SOLUSDT");
        assert_eq!(prices(event), Some((d("90.28"), d("90.30"))));
    }

    #[test]
    fn waits_until_both_sides_are_known() {
        let mut best = Best::default();
        best.on_message(r#"{"topic":"orderbook.1.SOLUSDT","ts":1,"type":"snapshot","data":{"b":[["90.28","1"]],"a":[["90.29","1"]]}}"#, "SOLUSDT");
        // The ask is gone (size 0): no price until it's back.
        let gone = best.on_message(r#"{"topic":"orderbook.1.SOLUSDT","ts":2,"type":"delta","data":{"b":[],"a":[["90.29","0"]]}}"#, "SOLUSDT");
        assert!(gone.is_none());
        let back = best.on_message(r#"{"topic":"orderbook.1.SOLUSDT","ts":3,"type":"delta","data":{"b":[],"a":[["90.31","2"]]}}"#, "SOLUSDT");
        assert_eq!(prices(back), Some((d("90.28"), d("90.31"))));
    }

    #[test]
    fn ignores_pongs_and_other_symbols() {
        let mut best = Best::default();
        assert!(
            best.on_message(
                r#"{"success":true,"ret_msg":"pong","op":"ping"}"#,
                "SOLUSDT"
            )
            .is_none()
        );
        assert!(best.on_message(r#"{"topic":"orderbook.1.ETHUSDT","ts":1,"type":"snapshot","data":{"b":[["3000","1"]],"a":[["3001","1"]]}}"#, "SOLUSDT").is_none());
    }
}

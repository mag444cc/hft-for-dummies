//! Past prices at startup, so the slow signals don't start from nothing.
//!
//! The channel needs two hours of prices and the 1-hour RSI about fifteen
//! hours before they say anything. Without history, every restart (or a
//! machine that reboots) switches them off for that long. So when a config
//! uses them, the bot downloads the last day of 1-minute closes once at
//! startup and hands them to the market maker as an [`Event::History`].
//!
//! Binance's futures candles come first (the same prices the bot follows).
//! Binance blocks some countries, so if that fails we use Hyperliquid's
//! candles for the same coin: a tiny bit different, but the shape of the
//! last day is the same. If both fail, the signals just warm up live.

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::config::Config;
use crate::events::{Bus, Event};

/// 1500 one-minute candles: 25 hours, enough for the 1-hour RSI.
const MINUTES: u64 = 1500;

/// Does this config use a signal that needs past prices?
pub fn needed(config: &Config) -> bool {
    config.channel_lean_bps.is_some()
        || config.rsi_fast_lean_bps.is_some()
        || config.rsi_slow_lean_bps.is_some()
        || config.macd_lean_bps.is_some()
}

/// Fetch the history in the background and send it to the bot.
pub fn fetch(binance_symbol: &str, bus: Bus) {
    let symbol = binance_symbol.to_uppercase();
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        let now_ms = now_ms();
        let closes = match binance(&http, &symbol).await {
            Ok(c) => Ok(c),
            Err(e) => {
                tracing::warn!("history: Binance candles failed ({e:#}), trying Hyperliquid");
                hyperliquid(&http, coin(&symbol), now_ms).await
            }
        };
        match closes {
            Ok(closes) => {
                let ages = ages(&closes, now_ms);
                tracing::info!("history: {} minutes of past prices loaded", ages.len());
                let _ = bus.send(Event::History(ages));
            }
            Err(e) => tracing::warn!("history: none loaded ({e:#}); slow signals warm up live"),
        }
    });
}

async fn binance(http: &reqwest::Client, symbol: &str) -> Result<Vec<(u64, Decimal)>> {
    let url = format!(
        "https://fapi.binance.com/fapi/v1/klines?symbol={symbol}&interval=1m&limit={MINUTES}"
    );
    let text = get(http.get(url)).await?;
    parse_binance(&text)
}

async fn hyperliquid(
    http: &reqwest::Client,
    coin: &str,
    now_ms: u64,
) -> Result<Vec<(u64, Decimal)>> {
    let body = json!({"type": "candleSnapshot", "req": {
        "coin": coin, "interval": "1m",
        "startTime": now_ms - MINUTES * 60_000, "endTime": now_ms,
    }});
    let text = get(http.post("https://api.hyperliquid.xyz/info").json(&body)).await?;
    parse_hyperliquid(&text)
}

async fn get(request: reqwest::RequestBuilder) -> Result<String> {
    let response = request
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    Ok(response.text().await?)
}

/// Binance klines: `[[open_ms, open, high, low, close, volume, close_ms, ...], ...]`.
/// Returns (close time ms, close).
fn parse_binance(text: &str) -> Result<Vec<(u64, Decimal)>> {
    let rows: Vec<Vec<Value>> = serde_json::from_str(text).context("Binance klines")?;
    rows.iter()
        .map(|r| {
            let close = r.get(4).and_then(Value::as_str).context("close")?;
            let close_ms = r.get(6).and_then(Value::as_u64).context("close time")?;
            Ok((close_ms, Decimal::from_str(close)?))
        })
        .collect()
}

/// Hyperliquid candles: `[{"t": open_ms, "T": close_ms, "c": "close", ...}, ...]`.
fn parse_hyperliquid(text: &str) -> Result<Vec<(u64, Decimal)>> {
    let rows: Vec<Value> = serde_json::from_str(text).context("Hyperliquid candles")?;
    rows.iter()
        .map(|r| {
            let close = r.get("c").and_then(Value::as_str).context("close")?;
            let close_ms = r.get("T").and_then(Value::as_u64).context("close time")?;
            Ok((close_ms, Decimal::from_str(close)?))
        })
        .collect()
}

/// "SOLUSDT" -> "SOL" (Hyperliquid names coins without the quote currency).
fn coin(binance_symbol: &str) -> &str {
    binance_symbol
        .strip_suffix("USDT")
        .unwrap_or(binance_symbol)
}

/// Closed candles only, oldest first, as (seconds ago, close).
fn ages(closes: &[(u64, Decimal)], now_ms: u64) -> Vec<(u64, Decimal)> {
    let mut out: Vec<(u64, Decimal)> = closes
        .iter()
        .filter(|(close_ms, _)| *close_ms <= now_ms) // the current minute is still open
        .map(|(close_ms, close)| ((now_ms - close_ms) / 1000, *close))
        .collect();
    out.sort_by_key(|(ago, _)| std::cmp::Reverse(*ago));
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn reads_binance_and_hyperliquid_candles() {
        let binance = r#"[[1794027600000,"89.90","89.95","89.85","89.91","1200.5",1794027659999,"107000",300,"600","53900","0"]]"#;
        assert_eq!(
            parse_binance(binance).unwrap(),
            vec![(1794027659999, d("89.91"))]
        );
        let hl = r#"[{"t":1794027600000,"T":1794027659999,"s":"SOL","i":"1m","o":"89.9","c":"89.92","h":"89.95","l":"89.85","v":"310.2","n":45}]"#;
        assert_eq!(
            parse_hyperliquid(hl).unwrap(),
            vec![(1794027659999, d("89.92"))]
        );
        assert!(parse_binance(r#"{"code":0,"msg":"restricted location"}"#).is_err());
        assert_eq!(coin("SOLUSDT"), "SOL");
    }

    #[test]
    fn keeps_closed_candles_oldest_first_as_ages() {
        let now = 1_000_000;
        let closes = [(940_000, d("1")), (880_000, d("2")), (1_060_000, d("3"))];
        // The candle closing in the future is still open: dropped.
        assert_eq!(ages(&closes, now), vec![(120, d("2")), (60, d("1"))]);
    }
}

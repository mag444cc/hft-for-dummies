//! Hyperliquid: a second place to trade, priced off the same Binance feed.
//!
//! Same job as `bullet.rs`, different exchange. What's different:
//!
//! - **Keys.** Hyperliquid uses Ethereum-style keys. You approve an *API
//!   wallet* for your account in the Hyperliquid app; like a Bullet delegate,
//!   it can trade but can't withdraw. Signing is in `hyperliquid_sign.rs`.
//! - **Prices.** No fixed tick: up to 5 significant figures (see
//!   [`Ticks::SignificantFigures`]). Orders must be worth at least $10.
//! - **The book.** Every message is the whole top of the book, so there is no
//!   copy to keep in sync (no `book.rs` needed).
//! - **Answers.** One request can succeed while single orders in it are
//!   rejected (a post-only order that would have crossed, say). We report those
//!   back one by one.
//! - **Expiry.** Orders don't expire on their own. Instead we keep a *dead
//!   man's switch* armed: "cancel all my orders in 30s", pushed back every time
//!   we requote. If the bot dies, its orders go 30s later.
//! - **Rate limits.** Each account gets 10,000 requests to start with, then one
//!   more for every $1 it trades. A small bot that requotes a lot runs out, so
//!   the Hyperliquid configs requote less often.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::book::Book;
use crate::config::Config;
use crate::events::{Bus, Event, Fill, Order, Side, Stamp};
use crate::executor::{Batch, Rejected, Venue};
use crate::hyperliquid_sign::{
    Cancel, CancelAction, CancelByCloid, CancelByCloidAction, Limit, Modify, ModifyAction,
    OrderAction, OrderType, OrderWire, ScheduleCancelAction, Wallet,
};
use crate::strategy::Ticks;

/// Hyperliquid refuses orders worth less than this (USD).
const MIN_ORDER_USD: Decimal = Decimal::TEN;

pub struct Hyperliquid {
    http: reqwest::Client,
    /// e.g. https://api.hyperliquid.xyz
    api: String,
    mainnet: bool,
    /// The market, e.g. "SOL", and its number in Hyperliquid's list.
    coin: String,
    asset: u32,
    /// The API wallet that signs, and the account it trades (`None` in dry run).
    wallet: Option<Wallet>,
    account: Option<String>,
    /// Which prices Hyperliquid accepts on this market.
    pub ticks: Ticks,
    quote_ttl: Duration,
    /// Nonces must never repeat, so each one is at least one more than the last.
    last_nonce: u64,
    /// When the dead man's switch fires unless we push it back (ms since 1970).
    switch_at_ms: Option<u64>,
    /// Hyperliquid wouldn't arm the switch; stop asking.
    switch_refused: bool,
}

pub async fn connect(config: &Config, bus: Bus) -> Result<Hyperliquid> {
    let api = match &config.api_url {
        Some(url) => url.trim_end_matches('/').to_string(),
        None if config.network == "testnet" => "https://api.hyperliquid-testnet.xyz".into(),
        None => "https://api.hyperliquid.xyz".into(),
    };
    let http = reqwest::Client::new();

    // Dry run needs no key: the order book is public.
    let wallet = if config.dry_run {
        None
    } else {
        Some(Wallet::load(&config.key_file)?)
    };
    let account = match &wallet {
        Some(w) => Some(trading_account(&http, &api, &config.network, w.address()).await?),
        None => None,
    };

    let meta: Meta = info(&http, &api, json!({"type": "meta"}))
        .await
        .context("reading Hyperliquid's markets")?;
    let (asset, market) = meta
        .universe
        .iter()
        .enumerate()
        .find(|(_, m)| m.name == config.symbol)
        .ok_or_else(|| {
            anyhow!(
                "Hyperliquid has no market called {:?} (it names them by coin: \"SOL\", \"ETH\", \"BTC\")",
                config.symbol
            )
        })?;
    if market.is_delisted {
        bail!("{} is delisted on Hyperliquid", config.symbol);
    }
    let step = Decimal::new(1, market.sz_decimals);
    if !(config.order_size % step).is_zero() {
        bail!(
            "order_size {} must be a multiple of {}'s size step {step}",
            config.order_size,
            config.symbol
        );
    }
    let mids: HashMap<String, Decimal> = info(&http, &api, json!({"type": "allMids"}))
        .await
        .context("reading Hyperliquid's prices")?;
    let mid = *mids
        .get(&config.symbol)
        .ok_or_else(|| anyhow!("no price for {} on Hyperliquid", config.symbol))?;
    if config.order_size * mid < MIN_ORDER_USD {
        bail!(
            "order_size {} {} is worth ${}, under Hyperliquid's ${MIN_ORDER_USD} minimum. Raise it \
             (and max_position with it).",
            config.order_size,
            config.symbol,
            (config.order_size * mid).round_dp(2)
        );
    }
    // Perps allow up to 6 decimals, minus the size's decimals.
    let ticks = Ticks::SignificantFigures {
        figures: 5,
        max_decimals: 6u32.saturating_sub(market.sz_decimals),
    };
    tracing::info!(coin = %config.symbol, %mid, %step, account = ?account, "hyperliquid: connected");

    // Two connections, as on Bullet: the book on one, our orders and fills on
    // the other, so a flood of book updates can't delay news of a fill.
    let ws = format!("{}/ws", api.replacen("http", "ws", 1));
    let coin = config.symbol.clone();
    spawn_socket(
        ws.clone(),
        vec![json!({"type": "l2Book", "coin": coin})],
        bus.clone(),
        move |msg| book_event(msg, &coin).into_iter().collect(),
    );
    if let Some(account) = &account {
        let coin = config.symbol.clone();
        spawn_socket(
            ws,
            vec![
                json!({"type": "orderUpdates", "user": account}),
                json!({"type": "userFills", "user": account}),
            ],
            bus,
            move |msg| user_events(msg, &coin),
        );
    }

    Ok(Hyperliquid {
        http,
        api,
        mainnet: config.network == "mainnet",
        coin: config.symbol.clone(),
        asset: asset as u32,
        wallet,
        account,
        ticks,
        quote_ttl: Duration::from_secs(config.quote_ttl_secs),
        last_nonce: 0,
        switch_at_ms: None,
        switch_refused: false,
    })
}

impl Venue for Hyperliquid {
    /// Up to three requests, in this order: cancels, moves, new orders.
    /// Every order is post-only ("Alo") and tagged with our client id.
    async fn submit(&mut self, batch: &Batch) -> Result<Rejected> {
        let mut rejected = Rejected::default();
        if !batch.cancels.is_empty() {
            let cancels = batch
                .cancels
                .iter()
                .map(|&id| CancelByCloid {
                    asset: self.asset,
                    cloid: cloid(id),
                })
                .collect();
            let statuses = self.post(&CancelByCloidAction::new(cancels)).await?;
            for status in &statuses {
                if let Some(error) = refused(status)
                    && !already_gone(error)
                {
                    tracing::warn!("hyperliquid: cancel refused: {error}");
                    rejected.cancel_failed = true;
                }
            }
        }
        if !batch.replaces.is_empty() {
            let modifies = batch
                .replaces
                .iter()
                .map(|(old, new)| Modify {
                    oid: cloid(*old),
                    order: self.wire(new),
                })
                .collect();
            let statuses = self.post(&ModifyAction::new(modifies)).await?;
            check_count(&statuses, batch.replaces.len())?;
            for ((_, new), status) in batch.replaces.iter().zip(&statuses) {
                if let Some(error) = refused(status) {
                    tracing::warn!("hyperliquid: couldn't move an order: {error}");
                    rejected.client_ids.push(new.client_id);
                    // Unless the old order is known to be gone, it may still rest.
                    if !already_gone(error) {
                        rejected.cancel_failed = true;
                    }
                }
            }
        }
        if !batch.places.is_empty() {
            let orders = batch.places.iter().map(|o| self.wire(o)).collect();
            let statuses = self.post(&OrderAction::new(orders)).await?;
            check_count(&statuses, batch.places.len())?;
            for (order, status) in batch.places.iter().zip(&statuses) {
                if let Some(error) = refused(status) {
                    tracing::warn!("hyperliquid: order refused: {error}");
                    rejected.client_ids.push(order.client_id);
                }
            }
        }
        if !batch.replaces.is_empty() || !batch.places.is_empty() {
            self.keep_switch_armed().await;
        }
        Ok(rejected)
    }

    /// Hyperliquid has no "cancel all", so: list our open orders on this
    /// coin, cancel each. Then switch the dead man's switch off, so a clean
    /// stop doesn't use up one of its 10 firings a day.
    async fn cancel_all(&mut self) -> Result<()> {
        let account = self.account()?.to_string();
        let open: Vec<OpenOrder> = self
            .info(json!({"type": "openOrders", "user": account}))
            .await?;
        let cancels: Vec<Cancel> = open
            .iter()
            .filter(|o| o.coin == self.coin)
            .map(|o| Cancel {
                a: self.asset,
                o: o.oid,
            })
            .collect();
        if !cancels.is_empty() {
            for status in self.post(&CancelAction::new(cancels)).await? {
                if let Some(error) = refused(&status)
                    && !already_gone(error)
                {
                    bail!("cancel refused: {error}");
                }
            }
        }
        if self.switch_at_ms.take().is_some() {
            self.post(&ScheduleCancelAction::new(None)).await?;
        }
        Ok(())
    }

    async fn position(&mut self) -> Result<Decimal> {
        let account = self.account()?.to_string();
        let state: ClearinghouseState = self
            .info(json!({"type": "clearinghouseState", "user": account}))
            .await
            .context("reading the account (has it been funded?)")?;
        Ok(state
            .asset_positions
            .iter()
            .find(|p| p.position.coin == self.coin)
            .map_or(Decimal::ZERO, |p| p.position.szi))
    }

    async fn fills_since(&mut self, since_unix_us: u64) -> Result<Vec<Fill>> {
        let account = self.account()?.to_string();
        let fills: Vec<HlFill> = self
            .info(json!({
                "type": "userFillsByTime",
                "user": account,
                "startTime": since_unix_us / 1000,
                "aggregateByTime": false,
            }))
            .await?;
        Ok(fills
            .iter()
            .filter(|f| f.coin == self.coin)
            .filter_map(HlFill::to_fill)
            .collect())
    }
}

impl Hyperliquid {
    fn account(&self) -> Result<&str> {
        self.account
            .as_deref()
            .ok_or_else(|| anyhow!("no account in dry run"))
    }

    /// One of our orders as Hyperliquid wants it.
    fn wire(&self, o: &Order) -> OrderWire {
        OrderWire {
            a: self.asset,
            b: o.side == Side::Buy,
            p: number(o.price),
            s: number(o.size),
            r: false,
            t: OrderType {
                limit: Limit { tif: "Alo" },
            },
            c: Some(cloid(o.client_id)),
        }
    }

    /// Push the dead man's switch back to `quote_ttl` from now, once it's
    /// half used up (the same rhythm as Bullet's order expiry).
    async fn keep_switch_armed(&mut self) {
        if self.switch_refused {
            return;
        }
        let now = unix_ms();
        let ttl = self.quote_ttl.as_millis() as u64;
        if self.switch_at_ms.is_some_and(|at| at > now + ttl / 2) {
            return;
        }
        let at = now + ttl;
        match self.post(&ScheduleCancelAction::new(Some(at))).await {
            Ok(_) => self.switch_at_ms = Some(at),
            Err(e) => {
                self.switch_refused = true;
                tracing::warn!(
                    "hyperliquid: couldn't arm the dead man's switch ({e}). If the bot dies \
                     without cancelling, its orders stay up until you cancel them in the app."
                );
            }
        }
    }

    /// Sign and send an action. Returns the per-order answers ("statuses").
    async fn post<A: serde::Serialize + Sync>(&mut self, action: &A) -> Result<Vec<Value>> {
        let nonce = unix_ms().max(self.last_nonce + 1);
        self.last_nonce = nonce;
        let wallet = self
            .wallet
            .as_ref()
            .ok_or_else(|| anyhow!("no key in dry run"))?;
        let signature = wallet.sign(action, nonce, self.mainnet)?;
        let body = json!({
            "action": action,
            "nonce": nonce,
            "signature": signature,
            "vaultAddress": null,
        });
        let answer: Value = self
            .http
            .post(format!("{}/exchange", self.api))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        statuses(answer)
    }

    async fn info<T: DeserializeOwned>(&self, request: Value) -> Result<T> {
        info(&self.http, &self.api, request).await
    }
}

/// Ask the public `/info` endpoint something.
async fn info<T: DeserializeOwned>(http: &reqwest::Client, api: &str, request: Value) -> Result<T> {
    Ok(http
        .post(format!("{api}/info"))
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// The account an API wallet trades for. Refuses any other key: the bot never
/// runs on an account's own key, which could withdraw.
async fn trading_account(
    http: &reqwest::Client,
    api: &str,
    network: &str,
    signer: &str,
) -> Result<String> {
    let role: Value = info(http, api, json!({"type": "userRole", "user": signer}))
        .await
        .context("checking the key's API wallet approval")?;
    let app = if network == "testnet" {
        "https://app.hyperliquid-testnet.xyz/API"
    } else {
        "https://app.hyperliquid.xyz/API"
    };
    match role["role"].as_str() {
        Some("agent") => {
            let account = role["data"]["user"]
                .as_str()
                .ok_or_else(|| anyhow!("userRole answer without an account: {role}"))?
                .to_string();
            tracing::info!(
                signer,
                account,
                "hyperliquid: key is an approved API wallet"
            );
            Ok(account)
        }
        Some("user") => bail!(
            "the key in your key file (address {signer}) is an account's own key. The bot only \
             trades with an API wallet, which can't withdraw. Make one with \
             `cargo run -- keygen --venue hyperliquid` and approve it at {app}"
        ),
        _ => bail!(
            "the key in your key file has address {signer}, and that isn't an approved API wallet.\n  \
             - Made it with keygen? Approve it here (paste the address, then Authorize):\n    {app}\n  \
             - Check network = \"{network}\" matches where you approved it, and that the approval \
               hasn't expired."
        ),
    }
}

/// The per-order answers in an `/exchange` reply, or the error.
fn statuses(answer: Value) -> Result<Vec<Value>> {
    if answer["status"] != "ok" {
        bail!("hyperliquid said: {}", answer["response"]);
    }
    Ok(match &answer["response"]["data"]["statuses"] {
        Value::Array(statuses) => statuses.clone(),
        _ => Vec::new(), // e.g. the dead man's switch: nothing per order
    })
}

/// The reason one order or cancel was refused, if it was.
fn refused(status: &Value) -> Option<&str> {
    status.get("error").and_then(Value::as_str)
}

/// "Order was never placed, already canceled, or filled": nothing to cancel.
fn already_gone(error: &str) -> bool {
    error.contains("canceled") || error.contains("filled")
}

/// One answer per order, or we can't tell which order an answer is for.
fn check_count(statuses: &[Value], sent: usize) -> Result<()> {
    if statuses.len() != sent {
        bail!("hyperliquid answered {} of {sent} orders", statuses.len());
    }
    Ok(())
}

/// Our u64 client id as Hyperliquid's 16-byte hex client order id.
fn cloid(client_id: u64) -> String {
    format!("0x{client_id:032x}")
}

/// Back again. `None` for ids that aren't ours (orders placed by hand).
fn parse_cloid(cloid: &str) -> Option<u64> {
    let number = u128::from_str_radix(cloid.strip_prefix("0x")?, 16).ok()?;
    u64::try_from(number).ok()
}

/// A price or size the way Hyperliquid wants it: no trailing zeros ("100", "0.05").
fn number(value: Decimal) -> String {
    value.normalize().to_string()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// What the websockets and `/info` send us.

#[derive(Deserialize)]
struct Meta {
    universe: Vec<Market>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Market {
    name: String,
    sz_decimals: u32,
    #[serde(default)]
    is_delisted: bool,
}

#[derive(Deserialize)]
struct OpenOrder {
    coin: String,
    oid: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearinghouseState {
    asset_positions: Vec<AssetPosition>,
}

#[derive(Deserialize)]
struct AssetPosition {
    position: Position,
}

#[derive(Deserialize)]
struct Position {
    coin: String,
    /// Signed size: positive = long.
    szi: Decimal,
}

#[derive(Deserialize)]
struct HlFill {
    coin: String,
    px: Decimal,
    sz: Decimal,
    /// "B" (we bought) or "A" (we sold).
    side: String,
    fee: Decimal,
    /// Trade id, unique per fill.
    tid: u64,
    #[serde(default)]
    cloid: Option<String>,
}

impl HlFill {
    fn to_fill(&self) -> Option<Fill> {
        let side = match self.side.as_str() {
            "B" => Side::Buy,
            "A" => Side::Sell,
            other => {
                tracing::error!("hyperliquid: fill with unknown side {other:?}, ignoring it");
                return None;
            }
        };
        Some(Fill {
            trade_id: self.tid,
            client_id: self.cloid.as_deref().and_then(parse_cloid),
            side,
            price: self.px,
            size: self.sz,
            fee: self.fee,
            // A fill doesn't say; the order update ("filled") does.
            order_finished: false,
        })
    }
}

#[derive(Deserialize)]
struct L2Book {
    coin: String,
    time: u64,
    levels: [Vec<Level>; 2],
}

#[derive(Deserialize)]
struct Level {
    px: Decimal,
    sz: Decimal,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderUpdate {
    order: UpdatedOrder,
    status: String,
    status_timestamp: u64,
}

#[derive(Deserialize)]
struct UpdatedOrder {
    coin: String,
    #[serde(default)]
    cloid: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserFills {
    #[serde(default)]
    is_snapshot: bool,
    fills: Vec<HlFill>,
}

/// A book message: the whole top of the book, every time.
fn book_event(msg: &Value, coin: &str) -> Option<Event> {
    if msg["channel"] != "l2Book" {
        return None;
    }
    let book: L2Book = match serde_json::from_value(msg["data"].clone()) {
        Ok(book) => book,
        Err(e) => {
            tracing::warn!("hyperliquid: bad book message: {e}");
            return None;
        }
    };
    if book.coin != coin {
        return None;
    }
    let [bids, asks] = &book.levels;
    let side = |levels: &[Level]| -> BTreeMap<Decimal, Decimal> {
        levels.iter().map(|l| (l.px, l.sz)).collect()
    };
    Some(Event::Book(
        Book {
            bids: side(bids),
            asks: side(asks),
        },
        Stamp::now(Some(book.time * 1000), None),
    ))
}

/// News about our own orders and fills.
fn user_events(msg: &Value, coin: &str) -> Vec<Event> {
    match msg["channel"].as_str() {
        Some("orderUpdates") => {
            let Ok(updates) = serde_json::from_value::<Vec<OrderUpdate>>(msg["data"].clone())
            else {
                tracing::warn!("hyperliquid: bad order update: {msg}");
                return Vec::new();
            };
            updates
                .iter()
                .filter(|u| u.order.coin == coin)
                .filter_map(|u| {
                    let client_id = parse_cloid(u.order.cloid.as_deref()?)?;
                    Some(match u.status.as_str() {
                        "open" => Event::OrderLive {
                            client_id,
                            stamp: Stamp::now(
                                Some(u.status_timestamp * 1000),
                                Some(u.status_timestamp * 1000),
                            ),
                        },
                        // filled, canceled, rejected, marginCanceled, ...: off the book.
                        _ => Event::OrderGone { client_id },
                    })
                })
                .collect()
        }
        Some("userFills") => {
            let Ok(fills) = serde_json::from_value::<UserFills>(msg["data"].clone()) else {
                tracing::error!("hyperliquid: unreadable fills, position may now be wrong: {msg}");
                return Vec::new();
            };
            // The first message repeats old fills; they're already in our position.
            if fills.is_snapshot {
                return Vec::new();
            }
            fills
                .fills
                .iter()
                .filter(|f| f.coin == coin)
                .filter_map(HlFill::to_fill)
                .map(Event::Fill)
                .collect()
        }
        _ => Vec::new(),
    }
}

/// We ping every second and Hyperliquid answers every ping, so this long
/// without hearing anything means the connection is dead: reconnect.
const DEAD_AFTER: Duration = Duration::from_secs(3);

/// Keep one websocket open with these subscriptions, forever, and send what
/// `handle` makes of each message to `bus`. After a drop, sends
/// [`Event::Reconnecting`] so the bot pulls its quotes and checks its fills.
fn spawn_socket(
    url: String,
    subscriptions: Vec<Value>,
    bus: Bus,
    mut handle: impl FnMut(&Value) -> Vec<Event> + Send + 'static,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match tokio::time::timeout(DEAD_AFTER, connect_async(&url)).await {
                Ok(Ok((mut ws, _))) => {
                    backoff = Duration::from_secs(1);
                    for subscription in &subscriptions {
                        let msg = json!({"method": "subscribe", "subscription": subscription});
                        let _ = ws.send(Message::Text(msg.to_string().into())).await;
                    }
                    let mut ping = tokio::time::interval(Duration::from_secs(1));
                    let mut last_heard = Instant::now();
                    loop {
                        let msg = tokio::select! {
                            msg = ws.next() => match msg {
                                Some(msg) => msg,
                                None => break,
                            },
                            _ = ping.tick() => {
                                if last_heard.elapsed() > DEAD_AFTER {
                                    tracing::warn!("hyperliquid: nothing heard for {}s", DEAD_AFTER.as_secs());
                                    break;
                                }
                                let ping = json!({"method": "ping"}).to_string();
                                if ws.send(Message::Text(ping.into())).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        };
                        last_heard = Instant::now();
                        match msg {
                            Ok(Message::Text(text)) => {
                                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                                    tracing::warn!("hyperliquid: bad message: {text}");
                                    continue;
                                };
                                for event in handle(&value) {
                                    if bus.send(event).is_err() {
                                        return; // the bot has shut down
                                    }
                                }
                            }
                            Ok(Message::Ping(payload)) => {
                                if ws.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Message::Close(_)) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    tracing::warn!("hyperliquid: disconnected, reconnecting");
                    if bus.send(Event::Reconnecting).is_err() {
                        return;
                    }
                }
                Ok(Err(e)) => tracing::warn!("hyperliquid: connect failed: {e}"),
                Err(_) => tracing::warn!("hyperliquid: connect timed out"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn client_ids_round_trip() {
        assert_eq!(cloid(1), "0x00000000000000000000000000000001");
        assert_eq!(
            parse_cloid(&cloid(1_793_276_669_648_035)),
            Some(1_793_276_669_648_035)
        );
        // Someone else's id, too big to be one of ours.
        assert_eq!(parse_cloid("0xffffffffffffffffffffffffffffffff"), None);
    }

    #[test]
    fn prices_go_out_without_trailing_zeros() {
        assert_eq!(number(d("100.00")), "100");
        assert_eq!(number(d("99.910")), "99.91");
        assert_eq!(number(d("0.05")), "0.05");
    }

    #[test]
    fn reads_the_book() {
        let msg = parse(
            r#"{"channel":"l2Book","data":{"coin":"SOL","time":1700000000000,"levels":[
                [{"px":"100.1","sz":"5","n":1},{"px":"100.0","sz":"2","n":1}],
                [{"px":"100.2","sz":"3","n":2}]]}}"#,
        );
        let Some(Event::Book(book, stamp)) = book_event(&msg, "SOL") else {
            panic!()
        };
        assert_eq!(
            (book.best_bid(), book.best_ask()),
            (Some(d("100.1")), Some(d("100.2")))
        );
        assert_eq!(stamp.exchange_unix_us, Some(1_700_000_000_000_000));
        assert!(book_event(&msg, "ETH").is_none(), "another coin's book");
        assert!(book_event(&parse(r#"{"channel":"pong"}"#), "SOL").is_none());
    }

    #[test]
    fn order_updates_become_live_and_gone() {
        let msg = parse(&format!(
            r#"{{"channel":"orderUpdates","data":[
                {{"order":{{"coin":"SOL","side":"B","limitPx":"99","sz":"0.1","oid":1,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"open","statusTimestamp":5}},
                {{"order":{{"coin":"SOL","side":"A","limitPx":"101","sz":"0","oid":2,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"filled","statusTimestamp":6}},
                {{"order":{{"coin":"SOL","side":"A","limitPx":"101","sz":"0.1","oid":3,"timestamp":1,"origSz":"0.1"}},"status":"canceled","statusTimestamp":7}},
                {{"order":{{"coin":"ETH","side":"A","limitPx":"3000","sz":"0.1","oid":4,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"canceled","statusTimestamp":8}}
            ]}}"#,
            cloid(7),
            cloid(8),
            cloid(9)
        ));
        let events = user_events(&msg, "SOL");
        assert_eq!(
            events.len(),
            2,
            "no cloid and other coins skipped: {events:?}"
        );
        assert!(matches!(events[0], Event::OrderLive { client_id: 7, .. }));
        assert!(matches!(events[1], Event::OrderGone { client_id: 8 }));
    }

    #[test]
    fn fills_count_but_the_snapshot_does_not() {
        let fill = format!(
            r#"{{"coin":"SOL","px":"100.52","sz":"0.15","side":"A","time":1,"startPosition":"0","dir":"Open Short","closedPnl":"0","hash":"0x0","oid":5,"crossed":false,"fee":"0.0023","tid":42,"cloid":"{}","feeToken":"USDC"}}"#,
            cloid(7)
        );
        let snapshot = parse(&format!(
            r#"{{"channel":"userFills","data":{{"isSnapshot":true,"user":"0x1","fills":[{fill}]}}}}"#
        ));
        assert!(user_events(&snapshot, "SOL").is_empty());

        let live = parse(&format!(
            r#"{{"channel":"userFills","data":{{"user":"0x1","fills":[{fill}]}}}}"#
        ));
        let events = user_events(&live, "SOL");
        let [Event::Fill(f)] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!(
            (f.trade_id, f.client_id, f.side, f.price, f.size, f.fee),
            (42, Some(7), Side::Sell, d("100.52"), d("0.15"), d("0.0023"))
        );
        assert!(user_events(&live, "BTC").is_empty(), "another coin");
    }

    #[test]
    fn reads_per_order_answers() {
        let ok = parse(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"resting":{"oid":77}},
                {"error":"Post only order would have immediately matched, bbo was 100.1@100.2. asset=5"}]}}}"#,
        );
        let answers = statuses(ok).unwrap();
        assert_eq!(refused(&answers[0]), None);
        assert!(refused(&answers[1]).is_some_and(|e| !already_gone(e)));

        let gone = parse(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[
                {"error":"Order was never placed, already canceled, or filled. asset=5"}]}}}"#,
        );
        assert!(refused(&statuses(gone).unwrap()[0]).is_some_and(already_gone));

        let failed =
            parse(r#"{"status":"err","response":"User or API Wallet 0x1 does not exist."}"#);
        assert!(statuses(failed).is_err());

        let switch = parse(r#"{"status":"ok","response":{"type":"default"}}"#);
        assert!(statuses(switch).unwrap().is_empty());
    }
}

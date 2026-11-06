//! Everything you can tune lives in `bot.toml`. This file reads it and checks
//! the numbers make sense before any money is involved.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where we trade: "bullet" (the default) or "hyperliquid".
    #[serde(default)]
    pub venue: VenueName,
    /// "mainnet" or "testnet".
    pub network: String,
    /// Talk to Bullet at this address instead of the network's public one,
    /// e.g. a direct endpoint for whitelisted market makers (Part 10).
    #[serde(default)]
    pub api_url: Option<String>,
    /// true: watch the market and log the quotes we *would* place. No key, no money.
    pub dry_run: bool,
    /// Key file made by `cargo run -- keygen`. Only needed when `dry_run = false`.
    pub key_file: PathBuf,
    /// Hyperliquid only: trade this sub-account of your account (its address,
    /// from the app's Sub-Accounts page) instead of the main one. Each
    /// sub-account has its own request allowance.
    #[serde(default)]
    pub sub_account: Option<String>,

    /// The market we quote: on Bullet e.g. "SOL-USD", on Hyperliquid the coin, e.g. "SOL".
    pub symbol: String,
    /// The Binance USD-M perp we borrow the price from, e.g. "SOLUSDT".
    pub binance_symbol: String,
    /// Also borrow the price from this Bybit USDT perp, e.g. "SOLUSDT":
    /// blended into fair value, and a fallback if Binance goes quiet. Off when unset.
    #[serde(default)]
    pub bybit_symbol: Option<String>,
    /// Bybit's share of fair value when both prices are fresh (0 to 1).
    #[serde(default = "default_bybit_weight")]
    pub bybit_weight: Decimal,

    /// Distance of each quote from fair value, in basis points (1 bp = 0.01%).
    pub spread_bps: Decimal,
    /// Size of each order, in the base asset (SOL for SOL-USD).
    pub order_size: Decimal,
    /// Never hold more than this, long or short.
    pub max_position: Decimal,
    /// How far (bps) both quotes lean against our position when it is at the max.
    pub skew_bps: Decimal,
    /// Widen the spread when the market is jumpy: quote at least this many
    /// times Binance's typical move over one `min_requote_ms` (measured over
    /// the last minute). `spread_bps` stays the minimum. Off when unset.
    #[serde(default)]
    pub vol_spread_mult: Option<Decimal>,
    /// Lean both quotes the way Binance just went: shift fair value by this
    /// fraction of Binance's move over the last `momentum_ms` (capped at
    /// 10 bps), so a quote isn't left where the price is heading. Off when unset.
    #[serde(default)]
    pub momentum_lean: Option<Decimal>,
    /// How far back `momentum_lean` looks.
    #[serde(default = "default_momentum_ms")]
    pub momentum_ms: u64,
    /// When recent fills are lopsided (say we keep getting sold to, so we
    /// keep buying), move that side's quote up to this many bps further
    /// away: someone may know the price is about to fall. Scaled by how
    /// lopsided they are; fills older than `fill_imbalance_secs` don't
    /// count. Off when unset.
    #[serde(default)]
    pub fill_imbalance_bps: Option<Decimal>,
    #[serde(default = "default_fill_imbalance_secs")]
    pub fill_imbalance_secs: u64,
    /// After Binance moves at least this many bps against our position
    /// within `big_move_secs`, stop skewing (don't cut the position cheaply):
    /// prices tend to snap back a little after a big move. Off when unset.
    #[serde(default)]
    pub big_move_bps: Option<Decimal>,
    #[serde(default = "default_big_move_secs")]
    pub big_move_secs: u64,
    /// If Binance has moved in a clean channel over the last `channel_secs`
    /// (a straight-line fit explains at least `channel_fit` of the moves,
    /// 0 to 1), lean both quotes up to this many bps towards the line: up
    /// near the lower edge, down near the upper edge, as prices tend to
    /// bounce between the edges. No lean once the channel breaks. Off when unset.
    #[serde(default)]
    pub channel_lean_bps: Option<Decimal>,
    #[serde(default = "default_channel_secs")]
    pub channel_secs: u64,
    #[serde(default = "default_channel_fit")]
    pub channel_fit: f64,
    /// Fade the channel lean out between two typical distances from the
    /// line and the break (three), instead of stopping it all at once.
    #[serde(default)]
    pub channel_fade: bool,
    /// Lean harder in cleaner channels: half of `channel_lean_bps` at the
    /// minimum `channel_fit`, all of it for a perfect line.
    #[serde(default)]
    pub channel_scale_by_fit: bool,
    /// Near a channel edge, hold a position the bounce favours (long near
    /// the bottom, short near the top) longer: weaken the skew by this
    /// fraction (0 to 1), and cut the other kind faster (strengthen it as
    /// much). Back to normal once the price has stayed out longer than the
    /// channel's own usual time back to the line, or the channel breaks.
    /// Needs `channel_lean_bps`. Off when unset.
    #[serde(default)]
    pub channel_hold: Option<Decimal>,
    /// When Binance's RSI on `rsi_fast_secs` candles is above 70
    /// (overbought), lean both quotes this many bps down: quick run-ups
    /// tended to fade over the next half hour. Off when unset.
    #[serde(default)]
    pub rsi_fast_lean_bps: Option<Decimal>,
    #[serde(default = "default_rsi_fast_secs")]
    pub rsi_fast_secs: u64,
    /// When Binance's RSI on `rsi_slow_secs` candles is below 30
    /// (oversold), lean both quotes this many bps up: hourly oversold
    /// markets tended to recover over the next hours. Off when unset.
    #[serde(default)]
    pub rsi_slow_lean_bps: Option<Decimal>,
    #[serde(default = "default_rsi_slow_secs")]
    pub rsi_slow_secs: u64,
    /// For six `macd_secs` candles after Binance's MACD turns (down from
    /// well above zero, or up from well below), lean both quotes this many
    /// bps that way. Off when unset.
    #[serde(default)]
    pub macd_lean_bps: Option<Decimal>,
    #[serde(default = "default_macd_secs")]
    pub macd_secs: u64,
    /// Order flow (CVD, see `flow.rs`): lean both quotes this many bps
    /// times who was more aggressive over the last `cvd_short_secs`, from
    /// -1 (only sellers) to +1 (only buyers). Negative values lean against
    /// the flow instead. Turns on Binance's perp trade feed. Off when unset.
    #[serde(default)]
    pub cvd_short_lean_bps: Option<Decimal>,
    #[serde(default = "default_cvd_short_secs")]
    pub cvd_short_secs: u64,
    /// The same over the last `cvd_long_secs`.
    #[serde(default)]
    pub cvd_long_lean_bps: Option<Decimal>,
    #[serde(default = "default_cvd_long_secs")]
    pub cvd_long_secs: u64,
    /// Also follow Bybit spot trades for this symbol (e.g. "SOLUSDT"); the
    /// CVD leans then use perps and spot together.
    #[serde(default)]
    pub cvd_spot_symbol: Option<String>,
    /// When perps and spot disagree over the long window (say perps buy
    /// while spot sells: often leverage, which tends to unwind), shrink the
    /// CVD lean by this fraction (0 to 1). Needs `cvd_spot_symbol`.
    #[serde(default)]
    pub cvd_divergence: Option<Decimal>,
    /// When spot is clearly buying (or selling) over the long window while
    /// perps are flat, lean this many extra bps with spot: real buyers taking
    /// coins, not leverage, tend to make steadier moves. Negative leans
    /// against spot instead (over 12 months of Binance data spot-led flow was
    /// slightly more often followed by a move the other way). Needs
    /// `cvd_spot_symbol`. Off when unset.
    #[serde(default)]
    pub cvd_spot_lead_bps: Option<Decimal>,
    /// When spot and perps are both clearly buying (or both selling) over
    /// the long window, lean this many bps that way. Negative leans against
    /// them instead: over 12 months of Binance data, flow everyone joined was
    /// followed by a move the other way 52-56% of the time (the clearest CVD
    /// signal). Needs `cvd_spot_symbol`. Off when unset.
    #[serde(default)]
    pub cvd_agree_bps: Option<Decimal>,
    /// The most all the fair-value leans together (momentum, channel, RSI,
    /// MACD, CVD) may shift fair value, either way, in bps: when many point
    /// the same way they add up, and past the spread the bot would be betting
    /// on direction rather than making a market. No limit when unset.
    #[serde(default)]
    pub max_total_lean_bps: Option<Decimal>,
    /// Extra spread (bps) on weekdays from `busy_start` to `busy_end`, New
    /// York time: around the US stock market open, the jumpiest hours of the
    /// crypto day (about 1.5 times the average over 2023-26). Following New
    /// York time keeps the window on the open through daylight saving
    /// (09:00-11:30 is 14:00-16:30 UTC in winter, 13:00-15:30 in summer).
    /// Off when unset.
    #[serde(default)]
    pub busy_spread_bps: Option<Decimal>,
    #[serde(default = "default_busy_start")]
    pub busy_start: String,
    #[serde(default = "default_busy_end")]
    pub busy_end: String,
    /// Spread change (bps) on Saturdays and Sundays (UTC), when markets are
    /// calmest: negative tightens. The spread never goes below 0.1 bps.
    /// Off when unset.
    #[serde(default)]
    pub weekend_spread_bps: Option<Decimal>,
    /// Extra spread (bps) around scheduled US news: inflation (CPI), jobs
    /// reports and Fed rate decisions, which can move crypto several percent
    /// in seconds. From `event_before_mins` before each event in
    /// `events_file` to `event_after_mins` after it. Off when unset.
    #[serde(default)]
    pub event_spread_bps: Option<Decimal>,
    #[serde(default = "default_event_before_mins")]
    pub event_before_mins: u64,
    #[serde(default = "default_event_after_mins")]
    pub event_after_mins: u64,
    /// The calendar: one "YYYY-MM-DD HH:MM name" per line, New York time.
    /// Relative to where the bot is started, like `key_file`.
    #[serde(default = "default_events_file")]
    pub events_file: PathBuf,
    /// The events from `events_file` (unix seconds, in order), read at load
    /// when `event_spread_bps` is set.
    #[serde(skip)]
    pub event_times: Vec<i64>,
    /// Only replace a resting quote once fair value has moved this far from it (bps).
    pub requote_threshold_bps: Decimal,
    /// And never more often than this...
    pub min_requote_ms: u64,
    /// ...unless fair value (which follows Binance) has moved this far (bps)
    /// *towards* one of our orders: then move that order at once, before
    /// the market reaches it. Off when unset.
    #[serde(default)]
    pub urgent_requote_bps: Option<Decimal>,
    /// Every order we place expires after this long, so if the bot dies its
    /// quotes stop being fillable. The bot refreshes them at half this age.
    pub quote_ttl_secs: u64,

    /// Dry run only: the fee each paper fill pays, in bps of its value, so
    /// the dry-run profit is after fees (Hyperliquid's base maker fee is 1.5).
    #[serde(default)]
    pub paper_fee_bps: Option<Decimal>,

    /// Stop quoting for the rest of the run once we are down this much (USD, fees included).
    pub max_loss_usd: Decimal,
    /// Pull quotes if Binance goes quiet for this long.
    pub reference_stale_secs: u64,
    /// Pull quotes if the Bullet book goes quiet for this long.
    pub book_stale_secs: u64,
    /// How slowly the Binance→Bullet price gap (the "basis") is averaged.
    pub basis_halflife_secs: u64,

    /// Serve latency metrics to Prometheus on this local port, for Grafana
    /// (`infra/grafana`). Off when unset.
    #[serde(default)]
    pub metrics_port: Option<u16>,
}

fn default_bybit_weight() -> Decimal {
    Decimal::new(3, 1)
}

fn default_momentum_ms() -> u64 {
    1000
}

fn default_fill_imbalance_secs() -> u64 {
    60
}

fn default_big_move_secs() -> u64 {
    300
}

fn default_busy_start() -> String {
    "09:00".into()
}

fn default_busy_end() -> String {
    "11:30".into()
}

fn default_event_before_mins() -> u64 {
    5
}

fn default_event_after_mins() -> u64 {
    15
}

fn default_events_file() -> PathBuf {
    "events.txt".into()
}

fn default_channel_secs() -> u64 {
    7200
}

fn default_channel_fit() -> f64 {
    0.7
}

fn default_rsi_fast_secs() -> u64 {
    300
}

fn default_rsi_slow_secs() -> u64 {
    3600
}

fn default_macd_secs() -> u64 {
    60
}

fn default_cvd_short_secs() -> u64 {
    60
}

fn default_cvd_long_secs() -> u64 {
    900
}

/// The exchanges the bot can trade on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VenueName {
    #[default]
    Bullet,
    Hyperliquid,
}

/// Read the event calendar (see `events_file`).
fn read_calendar(path: &Path) -> Result<Vec<i64>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the event calendar {}", path.display()))?;
    crate::clock::parse_calendar(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

impl Config {
    /// `--live` on the command line: trade for real without editing the file,
    /// so `bot.toml` never changes and every part's checkout stays clean.
    pub fn live(mut self, live: bool) -> Self {
        if live {
            self.dry_run = false;
        }
        self
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        if config.event_spread_bps.is_some() {
            config.event_times = read_calendar(&config.events_file)?;
        }
        Ok(config)
    }

    /// Whether any CVD lean is on (and so the trade feeds are needed).
    pub fn cvd_enabled(&self) -> bool {
        self.cvd_short_lean_bps.is_some()
            || self.cvd_long_lean_bps.is_some()
            || self.cvd_spot_lead_bps.is_some()
            || self.cvd_agree_bps.is_some()
    }

    fn validate(&self) -> Result<()> {
        if !matches!(self.network.as_str(), "mainnet" | "testnet") {
            bail!(
                "network must be \"mainnet\" or \"testnet\", got {:?}",
                self.network
            );
        }
        for (name, value) in [
            ("spread_bps", self.spread_bps),
            ("order_size", self.order_size),
            ("max_position", self.max_position),
            ("requote_threshold_bps", self.requote_threshold_bps),
            ("max_loss_usd", self.max_loss_usd),
        ] {
            if value <= Decimal::ZERO {
                bail!("{name} must be greater than 0, got {value}");
            }
        }
        if let Some(urgent) = self.urgent_requote_bps
            && urgent <= Decimal::ZERO
        {
            bail!("urgent_requote_bps must be greater than 0, got {urgent}");
        }
        if self.bybit_weight < Decimal::ZERO || self.bybit_weight > Decimal::ONE {
            bail!(
                "bybit_weight must be between 0 and 1, got {}",
                self.bybit_weight
            );
        }
        if self.momentum_lean.is_some_and(|m| m <= Decimal::ZERO) {
            bail!("momentum_lean must be greater than 0");
        }
        if self.momentum_lean.is_some() && self.momentum_ms == 0 {
            bail!("momentum_ms must be greater than 0");
        }
        if self.fill_imbalance_bps.is_some_and(|b| b <= Decimal::ZERO) {
            bail!("fill_imbalance_bps must be greater than 0");
        }
        if self.fill_imbalance_bps.is_some() && self.fill_imbalance_secs == 0 {
            bail!("fill_imbalance_secs must be greater than 0");
        }
        if self.big_move_bps.is_some_and(|b| b <= Decimal::ZERO) {
            bail!("big_move_bps must be greater than 0");
        }
        if self.big_move_bps.is_some() && self.big_move_secs == 0 {
            bail!("big_move_secs must be greater than 0");
        }
        if self.channel_lean_bps.is_some_and(|b| b <= Decimal::ZERO) {
            bail!("channel_lean_bps must be greater than 0");
        }
        if self.channel_lean_bps.is_some() && self.channel_secs < 600 {
            bail!("channel_secs must be at least 600 (ten minutes)");
        }
        if self
            .channel_hold
            .is_some_and(|h| h <= Decimal::ZERO || h > Decimal::ONE)
        {
            bail!("channel_hold must be above 0 and at most 1");
        }
        if self.channel_hold.is_some() && self.channel_lean_bps.is_none() {
            bail!("channel_hold needs channel_lean_bps (the channel it holds in)");
        }
        if !(0.0..1.0).contains(&self.channel_fit) {
            bail!("channel_fit must be at least 0 and below 1");
        }
        for (name, bps, secs) in [
            ("rsi_fast", self.rsi_fast_lean_bps, self.rsi_fast_secs),
            ("rsi_slow", self.rsi_slow_lean_bps, self.rsi_slow_secs),
            ("macd", self.macd_lean_bps, self.macd_secs),
        ] {
            if bps.is_some_and(|b| b <= Decimal::ZERO) {
                bail!("{name}_lean_bps must be greater than 0");
            }
            if bps.is_some() && secs < 10 {
                bail!("{name}_secs must be at least 10");
            }
        }
        if [self.cvd_short_lean_bps, self.cvd_long_lean_bps]
            .iter()
            .any(|b| b.is_some_and(|b| b.is_zero()))
        {
            bail!("a CVD lean of 0 does nothing; leave it out instead");
        }
        if self.cvd_enabled()
            && (self.cvd_short_secs == 0 || self.cvd_long_secs <= self.cvd_short_secs)
        {
            bail!("cvd_short_secs must be above 0 and below cvd_long_secs");
        }
        if self
            .cvd_divergence
            .is_some_and(|d| d <= Decimal::ZERO || d > Decimal::ONE)
        {
            bail!("cvd_divergence must be above 0 and at most 1");
        }
        if self.cvd_divergence.is_some() && self.cvd_spot_symbol.is_none() {
            bail!("cvd_divergence needs cvd_spot_symbol (spot to compare with)");
        }
        if self.cvd_spot_lead_bps.is_some_and(|b| b.is_zero()) {
            bail!("a cvd_spot_lead_bps of 0 does nothing; leave it out instead");
        }
        if self.cvd_spot_lead_bps.is_some() && self.cvd_spot_symbol.is_none() {
            bail!("cvd_spot_lead_bps needs cvd_spot_symbol (the spot trades it follows)");
        }
        if self.busy_spread_bps.is_some_and(|b| b <= Decimal::ZERO) {
            bail!("busy_spread_bps must be greater than 0");
        }
        match (
            crate::clock::parse_hhmm(&self.busy_start),
            crate::clock::parse_hhmm(&self.busy_end),
        ) {
            (Some(start), Some(end)) if start < end => {}
            _ => bail!(
                "busy_start and busy_end must be HH:MM (New York time), start before end; got {} and {}",
                self.busy_start,
                self.busy_end
            ),
        }
        if self.weekend_spread_bps.is_some_and(|b| b.is_zero()) {
            bail!("a weekend_spread_bps of 0 does nothing; leave it out instead");
        }
        if self.event_spread_bps.is_some_and(|b| b <= Decimal::ZERO) {
            bail!("event_spread_bps must be greater than 0");
        }
        if self.event_before_mins + self.event_after_mins == 0 {
            bail!("event_before_mins and event_after_mins can't both be 0");
        }
        if self.max_total_lean_bps.is_some_and(|b| b <= Decimal::ZERO) {
            bail!("max_total_lean_bps must be greater than 0");
        }
        if self.cvd_agree_bps.is_some_and(|b| b.is_zero()) {
            bail!("a cvd_agree_bps of 0 does nothing; leave it out instead");
        }
        if self.cvd_agree_bps.is_some() && self.cvd_spot_symbol.is_none() {
            bail!("cvd_agree_bps needs cvd_spot_symbol (spot to agree with)");
        }
        if self.vol_spread_mult.is_some_and(|m| m <= Decimal::ZERO) {
            bail!("vol_spread_mult must be greater than 0");
        }
        if self.paper_fee_bps.is_some_and(|fee| fee < Decimal::ZERO) {
            bail!("paper_fee_bps can't be negative");
        }
        if self.skew_bps < Decimal::ZERO {
            bail!("skew_bps can't be negative, got {}", self.skew_bps);
        }
        if self.order_size > self.max_position {
            bail!(
                "order_size ({}) is bigger than max_position ({}), so no order could ever be placed",
                self.order_size,
                self.max_position
            );
        }
        if let Some(sub_account) = &self.sub_account {
            if self.venue != VenueName::Hyperliquid {
                bail!("sub_account only works on Hyperliquid");
            }
            crate::hyperliquid_sign::address_bytes(sub_account)
                .context("sub_account must be the sub-account's address")?;
        }
        // Hyperliquid's dead man's switch can't be set less than 5s ahead.
        if self.venue == VenueName::Hyperliquid && self.quote_ttl_secs < 10 {
            bail!(
                "quote_ttl_secs must be at least 10 on Hyperliquid, got {}",
                self.quote_ttl_secs
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_bot_toml_is_valid_and_safe() {
        let config = Config::load(Path::new("bot.toml")).unwrap();
        assert!(config.dry_run, "a fresh clone must never trade real money");
    }

    #[test]
    fn live_from_the_command_line_only_turns_dry_run_off() {
        let config = Config::load(Path::new("bot.toml")).unwrap();
        assert!(config.live(false).dry_run, "no flag: the file decides");
        let config = Config::load(Path::new("bot.toml")).unwrap();
        assert!(!config.live(true).dry_run, "--live trades for real");
    }

    #[test]
    fn the_benchmark_config_is_valid() {
        assert!(!Config::load(Path::new("bench-live.toml")).unwrap().dry_run);
    }

    #[test]
    fn metrics_are_on_in_the_shipped_configs() {
        for path in ["bot.toml", "bench-live.toml"] {
            let config = Config::load(Path::new(path)).unwrap();
            assert_eq!(config.metrics_port, Some(9000), "{path}");
        }
    }

    #[test]
    fn rejects_an_order_bigger_than_the_position_limit() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        config.order_size = config.max_position + Decimal::ONE;
        assert!(config.validate().is_err());
    }

    /// Every config in `configs/`, by path.
    fn shipped_configs() -> Vec<std::path::PathBuf> {
        let mut paths: Vec<_> = std::fs::read_dir("configs")
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn every_shipped_config_is_valid_and_safe() {
        let paths = shipped_configs();
        assert!(paths.len() >= 5, "{paths:?}");
        for path in paths {
            let config =
                Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            assert!(config.dry_run, "{} must ship on dry run", path.display());
        }
    }

    #[test]
    fn several_bots_can_run_at_once() {
        let mut ports = vec![Config::load(Path::new("bot.toml")).unwrap().metrics_port];
        for path in shipped_configs() {
            ports.push(Config::load(&path).unwrap().metrics_port);
        }
        let count = ports.len();
        ports.sort();
        ports.dedup();
        assert_eq!(ports.len(), count, "each config needs its own metrics port");
    }

    #[test]
    fn hyperliquid_names_markets_by_coin() {
        for path in shipped_configs() {
            let config = Config::load(&path).unwrap();
            if config.venue == VenueName::Hyperliquid {
                assert!(!config.symbol.contains('-'), "{}", path.display());
                assert_eq!(config.binance_symbol, format!("{}USDT", config.symbol));
            }
        }
    }

    #[test]
    fn bullet_is_the_default_venue() {
        assert_eq!(
            Config::load(Path::new("bot.toml")).unwrap().venue,
            VenueName::Bullet
        );
    }

    #[test]
    fn a_sub_account_must_be_an_address_on_hyperliquid() {
        let mut config = Config::load(Path::new("configs/hyperliquid-sol.toml")).unwrap();
        config.sub_account = Some("0x1719884eb866cb12b2287399b15f7db5e7d775ea".into());
        assert!(config.validate().is_ok());
        config.sub_account = Some("my sub account".into());
        assert!(config.validate().is_err(), "not an address");

        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        config.sub_account = Some("0x1719884eb866cb12b2287399b15f7db5e7d775ea".into());
        assert!(
            config.validate().is_err(),
            "Bullet has no sub-accounts here"
        );
    }

    #[test]
    fn urgent_requotes_need_a_positive_threshold() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.urgent_requote_bps, None, "off unless asked for");
        config.urgent_requote_bps = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.urgent_requote_bps = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn bybit_is_off_unless_asked_for() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.bybit_symbol, None);
        assert_eq!(config.bybit_weight, Decimal::new(3, 1), "30% when it is on");
        config.bybit_weight = Decimal::new(11, 1);
        assert!(config.validate().is_err(), "more than 100%");
        config.bybit_weight = Decimal::NEGATIVE_ONE;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_momentum_lean_must_be_positive() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.momentum_lean, None, "off unless asked for");
        assert_eq!(config.momentum_ms, 1000);
        config.momentum_lean = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.momentum_lean = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
        config.momentum_ms = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn position_handling_is_off_unless_asked_for_and_must_be_positive() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.fill_imbalance_bps, None);
        assert_eq!(config.big_move_bps, None);
        assert_eq!(
            (config.fill_imbalance_secs, config.big_move_secs),
            (60, 300)
        );
        config.fill_imbalance_bps = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.fill_imbalance_bps = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
        config.big_move_bps = Some(Decimal::NEGATIVE_ONE);
        assert!(config.validate().is_err());
        config.big_move_bps = Some(Decimal::TEN);
        config.big_move_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_channel_lean_is_off_unless_asked_for() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.channel_lean_bps, None);
        assert_eq!((config.channel_secs, config.channel_fit), (7200, 0.7));
        assert!(!config.channel_fade && !config.channel_scale_by_fit);
        assert_eq!(config.channel_hold, None);
        config.channel_hold = Some(Decimal::new(5, 1));
        assert!(config.validate().is_err(), "needs a channel lean");
        config.channel_lean_bps = Some(Decimal::TWO);
        assert!(config.validate().is_ok());
        config.channel_hold = Some(Decimal::TWO);
        assert!(config.validate().is_err());
        config.channel_hold = None;
        config.channel_lean_bps = None;
        config.channel_lean_bps = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.channel_lean_bps = Some(Decimal::TWO);
        assert!(config.validate().is_ok());
        config.channel_secs = 60;
        assert!(config.validate().is_err());
        config.channel_secs = 7200;
        config.channel_fit = 1.5;
        assert!(config.validate().is_err());
    }

    #[test]
    fn indicator_leans_are_off_unless_asked_for() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(
            (
                config.rsi_fast_lean_bps,
                config.rsi_slow_lean_bps,
                config.macd_lean_bps
            ),
            (None, None, None)
        );
        assert_eq!(
            (config.rsi_fast_secs, config.rsi_slow_secs, config.macd_secs),
            (300, 3600, 60)
        );
        config.macd_lean_bps = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.macd_lean_bps = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
        config.macd_secs = 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn cvd_leans_are_off_unless_asked_for() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert!(!config.cvd_enabled());
        assert_eq!((config.cvd_short_secs, config.cvd_long_secs), (60, 900));
        config.cvd_long_lean_bps = Some(Decimal::NEGATIVE_ONE);
        assert!(
            config.cvd_enabled() && config.validate().is_ok(),
            "leaning against the flow is allowed"
        );
        config.cvd_divergence = Some(Decimal::new(5, 1));
        assert!(config.validate().is_err(), "needs spot");
        config.cvd_spot_symbol = Some("SOLUSDT".into());
        assert!(config.validate().is_ok());
        config.cvd_short_secs = 1000;
        assert!(
            config.validate().is_err(),
            "short must be shorter than long"
        );
        config.cvd_short_secs = 60;
        config.cvd_spot_lead_bps = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
        config.cvd_spot_symbol = None;
        config.cvd_divergence = None;
        assert!(config.validate().is_err(), "spot lead needs spot");
    }

    #[test]
    fn busy_hours_are_off_unless_asked_for() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(
            (config.busy_spread_bps, config.weekend_spread_bps),
            (None, None)
        );
        assert_eq!(
            (config.busy_start.as_str(), config.busy_end.as_str()),
            ("09:00", "11:30")
        );
        config.busy_spread_bps = Some(Decimal::ONE);
        config.weekend_spread_bps = Some(Decimal::NEGATIVE_ONE);
        assert!(config.validate().is_ok());
        config.busy_end = "08:00".into();
        assert!(config.validate().is_err(), "end before start");
        config.busy_end = "11:30".into();
        config.busy_spread_bps = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
    }

    #[test]
    fn scheduled_events_are_off_unless_asked_for() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.event_spread_bps, None);
        assert!(config.event_times.is_empty(), "no calendar read when off");
        assert_eq!((config.event_before_mins, config.event_after_mins), (5, 15));
        config.event_spread_bps = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
        config.event_spread_bps = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.event_spread_bps = Some(Decimal::ONE);
        config.event_before_mins = 0;
        config.event_after_mins = 0;
        assert!(config.validate().is_err(), "a window of nothing");
    }

    #[test]
    fn the_shipped_calendar_reads() {
        let events = read_calendar(Path::new("events.txt")).unwrap();
        assert!(events.len() >= 10, "{} events", events.len());
        assert!(read_calendar(Path::new("no-such-calendar.txt")).is_err());
    }

    #[test]
    fn the_volatility_multiplier_must_be_positive() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        assert_eq!(config.vol_spread_mult, None, "off unless asked for");
        config.vol_spread_mult = Some(Decimal::ZERO);
        assert!(config.validate().is_err());
        config.vol_spread_mult = Some(Decimal::ONE);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn the_paper_fee_cant_be_negative() {
        let mut config = Config::load(Path::new("configs/hyperliquid-sol.toml")).unwrap();
        assert_eq!(config.paper_fee_bps, Some(Decimal::new(15, 1)));
        config.paper_fee_bps = Some(Decimal::NEGATIVE_ONE);
        assert!(config.validate().is_err());
    }

    #[test]
    fn hyperliquid_needs_room_for_its_dead_mans_switch() {
        let mut config = Config::load(Path::new("configs/hyperliquid-sol.toml")).unwrap();
        config.quote_ttl_secs = 5;
        assert!(config.validate().is_err());
    }
}

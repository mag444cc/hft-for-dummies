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

fn default_channel_secs() -> u64 {
    7200
}

fn default_channel_fit() -> f64 {
    0.7
}

/// The exchanges the bot can trade on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VenueName {
    #[default]
    Bullet,
    Hyperliquid,
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
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
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

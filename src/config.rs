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

    /// Distance of each quote from fair value, in basis points (1 bp = 0.01%).
    pub spread_bps: Decimal,
    /// Size of each order, in the base asset (SOL for SOL-USD).
    pub order_size: Decimal,
    /// Never hold more than this, long or short.
    pub max_position: Decimal,
    /// How far (bps) both quotes lean against our position when it is at the max.
    pub skew_bps: Decimal,
    /// Only replace a resting quote once fair value has moved this far from it (bps).
    pub requote_threshold_bps: Decimal,
    /// And never more often than this.
    pub min_requote_ms: u64,
    /// Every order we place expires after this long, so if the bot dies its
    /// quotes stop being fillable. The bot refreshes them at half this age.
    pub quote_ttl_secs: u64,

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

/// The exchanges the bot can trade on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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
    fn hyperliquid_needs_room_for_its_dead_mans_switch() {
        let mut config = Config::load(Path::new("configs/hyperliquid-sol.toml")).unwrap();
        config.quote_ttl_secs = 5;
        assert!(config.validate().is_err());
    }
}

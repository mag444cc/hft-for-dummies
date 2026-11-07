# hft-for-dummies

Companion code for the *HFT for dummies* blog series: a SOL-USD market maker on
Bullet priced off Binance. It can also quote ETH and BTC, and trade on
Hyperliquid (`configs/`). Readers fork it and follow along part by part, many
of them new to Rust and to trading. `README.md` is the user-facing guide; this
file is for agents.

Two jobs come up. Read the section for yours.

## Helping someone set up and run the bot

Follow the README: it is the setup guide, for people and agents alike (Run
it, Going live). Don't restate its steps to the user from
memory; read them from there. If a step there is wrong or unclear, fix the
README. What follows is only what an agent needs on top.

The user is probably new to Rust, trading, or both. Go one step at a time,
say what each command does before running it, and explain the log as it
appears.

- **Going live is the user's call.** Don't run with `--live`, set
  `dry_run = false`, or push them towards it unless they've asked to trade
  for real. Confirm the amount, exchange and market first. Go live with
  `cargo run -- --live` (or `cargo run -- run configs/<name>.toml --live`),
  not by editing a config: the files stay on dry run and every part's
  checkout stays clean.
- **Keys.** Never ask for a private key in the chat, and never print, `cat`,
  copy, or commit `bot.key`. Prefer `keygen` and hand the user the link to
  confirm: the secret never leaves their disk. If they bring their own key,
  they paste it into `bot.key` themselves. Addresses are public and fine.
  The same goes for Hyperliquid's `hyperliquid.key`
  (`cargo run -- keygen --venue hyperliquid`); the user approves its address
  as an API wallet at https://app.hyperliquid.xyz/API.
- **The Bullet app is the user's.** They create the account, deposit and
  approve delegates there. Ask for their account address so you can check
  things read-only (no key needed; `ADDR` the account, `KEY` the delegate's
  address):

  ```sh
  U=https://tradingapi.bullet.xyz
  curl -s "$U/api/v1/delegateOf?address=KEY"                     # {"parent": ADDR} once delegated
  curl -s "$U/fapi/v3/account?address=ADDR"                      # balance, positions
  curl -s "$U/fapi/v1/openOrders?address=ADDR&symbol=SOL-USD"    # resting orders
  ```

  Full API docs (REST, WebSocket, order types, delegates), written for agents:
  https://tradingapi.bullet.xyz/docs/llms.txt

  On Hyperliquid (`ADDR` the account, `KEY` the API wallet's address):

  ```sh
  H=https://api.hyperliquid.xyz/info; J='content-type: application/json'
  curl -s $H -H "$J" -d '{"type":"userRole","user":"KEY"}'            # {"role":"agent","data":{"user":ADDR}} once approved
  curl -s $H -H "$J" -d '{"type":"clearinghouseState","user":"ADDR"}' # balance, positions
  curl -s $H -H "$J" -d '{"type":"openOrders","user":"ADDR"}'         # resting orders
  curl -s $H -H "$J" -d '{"type":"subAccounts","user":"ADDR"}'        # sub-accounts (null if none)
  ```

  With `sub_account` set in a config, `ADDR` for positions and orders is the
  sub-account's address.

  Run these before a live start (warn if there's already a SOL-USD position or
  orders on that market) and after stopping (no orders should remain; any position is the
  user's to close or keep).
- **Stopping.** Ctrl-C or SIGINT, so the bot cancels its orders. Never
  SIGKILL a live bot.

## Working on the code

### Commands

```sh
cargo run                     # run (dry run by default, reads bot.toml)
cargo run -- keygen           # make bot.key for live trading
cargo run -- record <config> <out.jsonl>            # record the market a config quotes
cargo run -- backtest <rec.jsonl> <configs or folder> --latency-ms 150
                              # replay it through configs; use a realistic order
                              # delay, or every strategy looks better than it is
cargo test                    # after every change
cargo fmt && cargo clippy --all-targets -- -D warnings   # before every commit (CI runs both)
```

### Architecture (keep it)

Event-driven, the same shape as the bullet-bots harness, kept small:

- Feeds (`binance.rs`, `bullet.rs`, `hyperliquid.rs`) turn the outside world into `Event`s on one bus (`events.rs`).
- `MarketMaker` (`market_maker.rs`) is a pure state machine: `on_event` / `requote` return `Intent`s. No I/O, no awaits.
- An `Executor` (`executor.rs`) carries intents out and reports results back as events. Live and paper implement the same trait.
  The live one talks to an exchange through the `Venue` trait, which Bullet and Hyperliquid implement.
- `bot.rs` only wires and runs the loop.
- Extra feeds, each only started when a config asks for it: `bybit.rs` (second price
  reference), `flow.rs` (Binance perp and Bybit spot trades, for CVD).
- Signals the market maker keeps itself, from Binance's price: `volatility.rs`,
  `momentum.rs`, `channel.rs` (two-hour trendline), `indicators.rs` (RSI, MACD on its
  own candles), `flow::Cvd`. `recording.rs` + `backtest.rs` replay recorded markets.

### Optional strategy settings (all off unless set in a config)

They either shift fair value (the "leans", which add up; `max_total_lean_bps` caps
the sum) or change one side or the skew. None is proven to make money yet: test a
setting with `backtest --latency-ms` on recordings before running it, and only run
it live if it helps across coins and days.

- Spread: `vol_spread_mult` (widen in jumpy markets), `busy_spread_bps` (around the US
  open, New York time), `weekend_spread_bps`, `event_spread_bps` (around scheduled US
  news from `events.txt`: CPI, jobs, Fed; keep the calendar's dates up to date).
- Leans: `momentum_lean` (last second of Binance), `channel_lean_bps` (+ `channel_fade`,
  `channel_scale_by_fit`), `rsi_fast_lean_bps` / `rsi_slow_lean_bps`, `macd_lean_bps`,
  `cvd_short_lean_bps` / `cvd_long_lean_bps` (negative = against the flow),
  `cvd_spot_lead_bps`, `cvd_agree_bps`, `cvd_divergence`; capped by `max_total_lean_bps`.
- Position handling: `fill_imbalance_bps` (push the side that keeps getting hit
  away), `big_move_bps` (no skew after a big move against us), `channel_hold`,
  `jump_bps` (after a fast Binance move, step back the side it heads for).
- Binance splits its futures streams: book streams (bookTicker) under `/public`,
  trades and liquidations under `/market`; the old `/ws` path no longer carries trades.

Each series part should change one box and leave the others alone. When a part
needs a new capability, add an event, intent, or executor, not a special case
in the loop. Don't add generality no part needs yet (several
strategies, actor registries, several markets in one process: run one bot per
market instead).

### Rules

- One series part = one PR (several if the part is big), squash-merged and tagged `part-N`.
  The PR adds `docs/parts/NN-name.md` (what it adds, how to try it, link to
  `compare/part-(N-1)...part-N`) and links it from the README's series table.
  The README itself only describes the current bot; part-specific notes go in the guide.
  PR titles are Conventional Commits (`feat(executor): ...`).
- Write for beginners: plain comments that explain *why*, short files, no clever tricks.
- `rust_decimal::Decimal` for every price, size and amount.
- Position changes only from fills (`Inventory::on_fill`) or an exchange read (`adopt`), never from sending an order.
- Money-safety code needs a test for every change: `market_maker.rs`, `strategy.rs`,
  `inventory.rs`, `executor.rs`, `book.rs`, `fair_value.rs`, `key.rs`, `hyperliquid_sign.rs`,
  the message parsing in `hyperliquid.rs`, and the config checks.
  Write the failing test first.
- `bot.toml` and every file in `configs/` must ship with `dry_run = true` (tests enforce it). Never commit keys (`*.key` is ignored).
- Keep the README's setup steps and the section above in step with the code.

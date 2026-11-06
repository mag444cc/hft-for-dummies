# Experiment: which spread and requote speed works best?

42 dry-run bots on Hyperliquid, priced off Binance: 14 setups, each on SOL,
ETH and BTC. Paper orders queue and fill like real ones, and pay Hyperliquid's
base maker fee (1.5 bps), so the profit they log is after fees. No key, no
money.

| Setup | Spread (bps) | Fastest requote | Urgent requotes |
|-------|--------------|-----------------|-----------------|
| A / Au | 1 | 0.4 s | – / 1 bp |
| G / Gu | 1.5 | 0.4 s | – / 1.5 bps |
| F / Fu | 2 | 0.4 s | – / 2 bps |
| En / E | 2 | 1 s | – / 3 bps |
| B / Bu | 2 | 2 s | – / 2 bps |
| C / Cu | 3 | 1 s | – / 3 bps |
| D / Du | 4 | 1 s | – / 4 bps |

Each pair differs only in urgent requotes: moving an order at once when
Binance moves towards it, instead of waiting out `min_requote_ms`.

Run it on the Tokyo server (Part 4), where nothing sleeps:

```sh
./infra/bot/deploy.sh experiment 8   # start all 42 for 8 hours
./infra/bot/deploy.sh results        # the comparison, any time
./infra/bot/deploy.sh destroy        # when done: delete the server
```

Live, these settings would need more requests than one Hyperliquid account's
allowance (see the README); in dry run nothing is sent.

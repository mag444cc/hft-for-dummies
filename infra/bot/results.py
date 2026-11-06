#!/usr/bin/env python3
"""Summarise the experiment's bots from their logs, on the server.

Run by `deploy.sh results`. For each setup: profit (after the paper fee) per
coin, trades, fees, requests a minute, and whether a bot hit its loss limit.
A bot that restarted has several runs in its log; they are added up.
"""
import glob
import os
import re
import subprocess
from datetime import datetime

FEE = 0.00015  # the paper fee in the configs, to show profit before fees too
COINS = ["sol", "eth", "btc"]


def runs(name):
    """The bot's log, split into one list of lines per run."""
    log = subprocess.run(
        ["journalctl", "-u", f"hft-exp@{name}", "-o", "cat", "--no-pager"],
        capture_output=True, text=True,
    ).stdout.splitlines()
    out = []
    for line in log:
        if "DRY RUN:" in line or not out:
            out.append([])
        out[-1].append(line)
    return out


def when(line):
    return datetime.fromisoformat(line[:26])


def bot(name):
    trades = 0
    fees = pnl = requests = minutes = 0.0
    stopped = False
    for lines in runs(name):
        summaries = [l for l in lines if "market_maker: mid " in l and re.search(r"pnl \$-?\d", l)]
        if not summaries:
            continue
        fills = [re.search(r"FILL: \w+ ([\d.]+) @ ([\d.]+)", l).groups() for l in lines if "FILL:" in l]
        trades += len(fills)
        fees += sum(float(size) * float(price) for size, price in fills) * FEE
        pnl += float(re.search(r"pnl \$(-?[\d.]+)", summaries[-1]).group(1))
        requests += sum(1 for l in lines if "paper: place" in l or "paper: replace" in l)
        stamped = [l for l in lines if l[:2] == "20" and l[4:5] == "-"]
        quoting = next((l for l in lines if l.endswith("market_maker: quoting")), None)
        if quoting and stamped:
            minutes += (when(stamped[-1]) - when(quoting)).total_seconds() / 60
        stopped |= any("loss limit" in l for l in lines)
    return trades, pnl, fees, requests, minutes, stopped


def main():
    here = os.path.expanduser("~/hft/configs/experiment")
    setups = sorted({os.path.basename(p).rsplit("-", 1)[0] for p in glob.glob(f"{here}/*.toml")})
    print(f"{'setup':6} {'SOL':>8} {'ETH':>8} {'BTC':>8} | {'trades':>6} {'before fees':>11} {'fees':>7} "
          f"{'AFTER FEES':>10} {'req/min/bot':>11} {'hours':>5}  loss-limit stops")
    for setup in setups:
        rows = {c: bot(f"{setup}-{c}") for c in COINS}
        trades = sum(r[0] for r in rows.values())
        net = sum(r[1] for r in rows.values())
        fees = sum(r[2] for r in rows.values())
        minutes = sum(r[4] for r in rows.values())
        per_min = sum(r[3] for r in rows.values()) / minutes if minutes else 0
        stops = ",".join(c for c, r in rows.items() if r[5]) or "-"
        print(f"{setup:6} {rows['sol'][1]:8.3f} {rows['eth'][1]:8.3f} {rows['btc'][1]:8.3f} | {trades:6} "
              f"{net + fees:11.3f} {fees:7.3f} {net:10.3f} {per_min:11.1f} {rows['sol'][4] / 60:5.1f}  {stops}")
    problems = subprocess.run(
        "journalctl -u 'hft-exp@*' -o cat --no-pager | grep -cE ' (WARN|ERROR) '",
        shell=True, capture_output=True, text=True,
    ).stdout.strip()
    print(f"\nwarnings and errors across all bots: {problems or 0}")
    print("(details: ./infra/bot/deploy.sh problems)")


if __name__ == "__main__":
    main()

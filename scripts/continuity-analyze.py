#!/usr/bin/env python3
"""One line per continuity run: what the server rolled back, what the clients
corrected and how their remote clock coped, and the packets that crossed the
veth. Then the run's rocket and pistol totals, server against clients. Usage:
scripts/continuity-analyze.py <log dir>...

Strict: a rollback or remote clock line that doesn't parse in full stops it
with a non-zero exit, so a changed log line can't quietly count as zero. A run
whose logs lack rocket totals, or whose server and clients disagree on rockets
fired or exploded or on pistol shots fired, prints everything and then exits 1.
Pistol hits are printed, not checked. A run with no pistol totals in any log
is noted, not failed; one where only some logs have them fails. Last, the
transports each client's sessions ran on, beside the one the run asked for."""
from datetime import datetime
import os
import re
import sys

strip = lambda line: re.sub(r"\x1b\[[0-9;]*m", "", line).rstrip("\n")

ROLLBACK = re.compile(r"input rollback, last 30s: (\d+) rollbacks, (\d+) re-stepped ticks, max depth (\d+), "
                      r"(\d+) external-edit floors, (\d+) re-anchors \((\d+) skipped\)$")
BURST = re.compile(r"max burst (\d+)\); (\d+) extrapolated frames \((\d+) capped\), (\d+) resets")
ROCKETS = re.compile(r"rockets: fired (\d+), exploded (\d+)$")
PISTOL = re.compile(r"pistol: fired (\d+), hit (\d+)$")
CONNECTED = re.compile(r"connected over (\w+)")
# Per weapon: its name in the report and in a mismatch, its totals line, its two counts, how
# many of them must agree, and whether a run without any of its lines fails.
WEAPONS = (("rockets", "ROCKET", ROCKETS, ("fired", "exploded"), 2, True),
           ("pistol", "PISTOL", PISTOL, ("fired", "hit"), 1, False))


def lines(path):
    return [strip(l) for l in open(path, errors="replace")] if os.path.exists(path) else []


def parse(path, line, marker, regex):
    """`regex` on a line that has `marker`; a line that has it but doesn't match ends the run."""
    if marker not in line:
        return None
    m = regex.search(line)
    if not m:
        sys.exit(f"{path}: unparsed line: {line}")
    return m


def shot_totals(path, regex):
    """{10 s wall-clock window: (fired, landed)}. Every process logs its totals as the
    machine's clock crosses a multiple of 10 s, so one window is the same instant in each."""
    totals = {}
    for l in lines(path):
        m = regex.search(l)
        if m:
            when = datetime.fromisoformat(l.split()[0]).timestamp()
            totals[int(when // 10)] = (int(m[1]), int(m[2]))
    return totals


def netem(path):
    """Packets sent each way, from `tc -s qdisc` (clients->server, server->clients)."""
    sent = re.findall(r"Sent \d+ bytes (\d+) pkt", "\n".join(lines(path)))
    return [int(n) for n in sent[:2]] if len(sent) >= 2 else [0, 0]


print("run | secs | server: rollbacks, re-stepped ticks, max depth | clients: mispredicts, resyncs | "
      "remote clock: extrapolated (capped), resets, max burst | pkt/s per client up, down")
shots = {name: [] for name, *_ in WEAPONS}
transports = []
failed = False
for run in sys.argv[1:]:
    name = os.path.basename(run.rstrip("/"))
    rollbacks = []
    for l in lines(f"{run}/server.log"):
        m = parse(f"{run}/server.log", l, "input rollback, last 30s", ROLLBACK)
        if m:
            rollbacks.append(tuple(map(int, m.groups())))
        parse(f"{run}/server.log", l, "remote clock, last", BURST)
    clients = sorted(f for f in os.listdir(run) if re.fullmatch(r"client\d+\.log", f))
    mispredicts = resyncs = extrapolated = capped = resets = burst = 0
    for client in clients:
        path = f"{run}/{client}"
        for l in lines(path):
            mispredicts += "mispredict ack" in l
            resyncs += "command clock resync" in l
            parse(path, l, "input rollback, last 30s", ROLLBACK)
            m = parse(path, l, "remote clock, last", BURST)
            if m:
                burst = max(burst, int(m[1]))
                extrapolated += int(m[2])
                capped += int(m[3])
                resets += int(m[4])
    secs = int((lines(f"{run}/seconds") or ["0"])[0])
    up, down = netem(f"{run}/netem-stats.txt")
    per = max(1, secs) * max(1, len(clients))
    total = lambda i: sum(r[i] for r in rollbacks)
    print(f"{name} | {secs} | {total(0)}, {total(1)}, "
          f"{max((r[2] for r in rollbacks), default=0)} | {mispredicts}, {resyncs} | "
          f"{extrapolated} ({capped}), {resets}, {burst} | {up / per:.0f}, {down / per:.0f}")

    # Each client's transports in the order its sessions came up, repeats folded.
    used = []
    for client in clients:
        order = []
        for l in lines(f"{run}/{client}"):
            m = CONNECTED.search(l)
            if m and m[1] not in order:
                order.append(m[1])
        used.append(f"{client[:-4]} {', '.join(order) or 'never connected'}")
    asked = (lines(f"{run}/transport") or ["?"])[0]
    transports.append(f"{name} | asked {asked} | " + " | ".join(used))

    # The last totals every log has: the server's against the sum of its clients'.
    for weapon, label, regex, counts, checked, required in WEAPONS:
        out = shots[weapon]
        logs = [shot_totals(f"{run}/server.log", regex)]
        logs += [shot_totals(f"{run}/{c}", regex) for c in clients]
        if not required and not any(logs):
            out.append(f"{name} | no {weapon} totals in any log")
            continue
        common = set.intersection(*(set(log) for log in logs))
        if not common or not clients:
            out.append(f"{name} | no {weapon} totals in every log")
            failed = True
            continue
        window = max(common)
        server, mine = logs[0][window], [log[window] for log in logs[1:]]
        theirs = [sum(m[i] for m in mine) for i in range(2)]
        out.append(f"{name} | server {counts[0]} {server[0]}, {counts[1]} {server[1]} | "
                   f"clients {counts[0]} {theirs[0]}, {counts[1]} {theirs[1]}")
        # A shot each client fired, or saw land, that the server hadn't stepped yet.
        for what, ours, their in list(zip(counts, server, theirs))[:checked]:
            if abs(ours - their) > 2 * len(clients):
                out.append(f"{name} | {label} MISMATCH: server {what} {ours}, "
                           f"clients {what} {their}")
                failed = True

for weapon, *_ in WEAPONS:
    print(f"\n{weapon} (the last totals every log has)")
    print("\n".join(shots[weapon]))
print("\ntransport (asked, then each client's sessions)")
print("\n".join(transports))
sys.exit(1 if failed else 0)

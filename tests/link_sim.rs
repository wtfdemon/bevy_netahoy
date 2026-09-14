//! Deterministic link-condition simulation of the usercmd pipeline.
//!
//! Reproduces "clumsy" style network abuse (delay + jitter + loss + reorder)
//! against the real server queue and its one-command reserve.
//!
//! - `Datagram`: WebTransport-like. Real wire loss and reorder, plus a
//!   congestion stall after each wire loss (QUIC collapses its window on
//!   loss). The sender's queue is culled by the vendored age expiry.
//! - `Stream`: WebSocket-like. The wire never loses or reorders; loss becomes
//!   a retransmit + head-of-line stall shared per RTT window. The sender
//!   drops at the door above `buffered_cap` in-flight bytes (bufferedAmount).
//!
//! Metrics are proxies for what the player sees:
//! - `skipped` sequences = input the server never executed = a forced rewind
//!   with real position error = a visible rollback/snap.
//! - `ack lag` = how stale the server's acks are = Dropped/resync-park risk
//!   (the original bufferbloat death spiral shows up here as unbounded lag).
//!
//! This deliberately does NOT simulate physics or reconcile itself — it
//! answers which transport/knob combinations feed the reconciler gap-free,
//! fresh input, which is the precondition for the smooth path.

use bevy_netahoy::protocol::{AhoyUserCmd, AhoyUserCmdPacket, FIXED_TIMESTEP_HZ};
use bevy_netahoy::server::QueuedUserCmds;
use std::collections::VecDeque;

const TICK_MS: f64 = 1000.0 / FIXED_TIMESTEP_HZ;
const TICKS_PER_SEC: u32 = FIXED_TIMESTEP_HZ as u32;
const SIM_TICKS: u32 = TICKS_PER_SEC * 300; // 5 simulated minutes
const WARMUP_TICKS: u32 = TICKS_PER_SEC; // ignore metrics during the first second
const CMD_BYTES: usize = 40; // from wire_size.rs

struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

#[derive(Clone, Copy)]
enum Link {
    Datagram {
        /// `None` models the pre-patch unbounded send queue.
        age_expiry_ms: Option<f64>,
        /// After a wire loss the congestion window collapses for this long;
        /// during collapse the writer trickles at `collapsed_rate` instead of
        /// stopping entirely (QUIC keeps a pacing floor).
        stall_ms: f64,
        /// Packets per tick while healthy.
        healthy_rate: f64,
        /// Packets per tick while collapsed (e.g. 0.125 = 8 pkt/s).
        collapsed_rate: f64,
    },
    Stream {
        /// bufferedAmount-style door: drop when this many bytes are in flight.
        buffered_cap: usize,
        /// Uplink serialization rate in bytes/ms; `None` = infinite pipe.
        /// This is where packet SIZE matters: redundancy that exceeds the
        /// pipe becomes self-induced bufferbloat.
        bandwidth: Option<f64>,
    },
}

#[derive(Clone, Copy)]
struct Cfg {
    label: &'static str,
    backups: u32,
    delay_ms: f64,
    jitter_ms: f64,
    loss: f64,
    link: Link,
}

#[derive(Default)]
struct Report {
    skipped: u32,
    max_gap: u32,
    ack_lag_p95: u32,
    ack_lag_max: u32,
    expired: u32,
    door_dropped: u32,
    wire_lost: u32,
    delivered: u32,
    starved_ticks: u32,
}

fn packet_for(newest: u32, backups: u32) -> AhoyUserCmdPacket {
    let oldest = newest.saturating_sub(backups - 1).max(1);
    AhoyUserCmdPacket {
        commands: (oldest..=newest)
            .map(|sequence| AhoyUserCmd {
                sequence,
                ..Default::default()
            })
            .collect(),
    }
}

fn simulate(cfg: Cfg, seed: u64) -> Report {
    let mut rng = Rng(seed);
    let mut report = Report::default();

    // (enqueued_at_ms, newest_seq, bytes)
    let mut send_queue: VecDeque<(f64, u32, usize)> = VecDeque::new();
    // (arrival_ms, newest_seq, bytes) — unsorted; drained by arrival time.
    let mut in_flight: Vec<(f64, u32, usize)> = Vec::new();
    let mut in_flight_bytes: usize = 0;

    let mut stalled_until = 0.0_f64;
    let mut send_credit = 0.0_f64;
    let mut hol_until = 0.0_f64;
    let mut last_arrival = 0.0_f64;
    let mut wire_busy_until = 0.0_f64;

    let mut queued = QueuedUserCmds::default();
    let mut last_processed: u32 = 0;
    let mut lag_samples: Vec<u32> = Vec::new();

    for tick in 1..=SIM_TICKS {
        let now = f64::from(tick) * TICK_MS;
        let seq = tick;
        let bytes = cfg.backups as usize * CMD_BYTES;
        send_queue.push_back((now, seq, bytes));

        // --- sender / wire model ---
        match cfg.link {
            Link::Datagram {
                age_expiry_ms,
                stall_ms,
                healthy_rate,
                collapsed_rate,
            } => {
                send_credit += if now < stalled_until {
                    collapsed_rate
                } else {
                    healthy_rate
                };
                send_credit = send_credit.min(healthy_rate.max(1.0));
                while send_credit >= 1.0 {
                    let Some((enqueued_at, newest, bytes)) = send_queue.pop_front() else {
                        break;
                    };
                    if let Some(age) = age_expiry_ms
                        && now - enqueued_at > age
                    {
                        report.expired += 1;
                        continue;
                    }
                    send_credit -= 1.0;
                    if rng.chance(cfg.loss) {
                        report.wire_lost += 1;
                        stalled_until = now + stall_ms;
                    } else {
                        // Independent per-packet jitter => natural reordering.
                        let arrival = now + cfg.delay_ms + rng.next_f64() * cfg.jitter_ms;
                        in_flight.push((arrival, newest, bytes));
                    }
                }
            }
            Link::Stream {
                buffered_cap,
                bandwidth,
            } => {
                // bufferedAmount only grows once the kernel/network pipe is
                // full: bytes the pipe absorbs in one one-way delay never
                // appear in the browser's counter. Model the door as cap +
                // baseline BDP rather than raw unacked bytes.
                let production_rate = (cfg.backups as usize * CMD_BYTES) as f64 / TICK_MS;
                let pipe_rate = bandwidth.unwrap_or(f64::INFINITY).min(production_rate);
                let door = buffered_cap.saturating_add((pipe_rate * cfg.delay_ms) as usize);
                while let Some((_, newest, bytes)) = send_queue.pop_front() {
                    if in_flight_bytes + bytes > door {
                        report.door_dropped += 1;
                        continue;
                    }
                    // Serialization onto a finite pipe: bytes queue behind
                    // everything already being transmitted.
                    if let Some(rate) = bandwidth {
                        wire_busy_until = wire_busy_until.max(now) + bytes as f64 / rate;
                    }
                    let base =
                        wire_busy_until.max(now) + cfg.delay_ms + rng.next_f64() * cfg.jitter_ms;
                    // One retransmit/HOL recovery per RTT window: a loss only
                    // extends the stall if we're not already inside one.
                    if rng.chance(cfg.loss) && base > hol_until {
                        hol_until = base + 2.0 * cfg.delay_ms;
                    }
                    // In-order delivery: never before the previous packet.
                    let arrival = base.max(hol_until).max(last_arrival);
                    last_arrival = arrival;
                    in_flight.push((arrival, newest, bytes));
                    in_flight_bytes += bytes;
                }
            }
        }

        // --- deliveries into the REAL server queue ---
        let mut index = 0;
        while index < in_flight.len() {
            if in_flight[index].0 <= now {
                let (_, newest, bytes) = in_flight.swap_remove(index);
                if matches!(cfg.link, Link::Stream { .. }) {
                    in_flight_bytes -= bytes;
                }
                queued.push_packet(&packet_for(newest, cfg.backups), last_processed);
                report.delivered += 1;
            } else {
                index += 1;
            }
        }

        // --- Server consumption policy: exactly one simulation step per
        // tick, freshest command wins. Empty queue repeats last intent
        // without inventing an acknowledgement; recovery discards obsolete
        // states.
        if let Some(command) = queued.pop_for_tick() {
            if tick > WARMUP_TICKS && command.sequence > last_processed + 1 {
                let gap = command.sequence - last_processed - 1;
                report.skipped += gap;
                report.max_gap = report.max_gap.max(gap);
            }
            last_processed = last_processed.max(command.sequence);
        } else {
            if tick > WARMUP_TICKS {
                report.starved_ticks += 1;
            }
        }

        if tick > WARMUP_TICKS {
            lag_samples.push(seq.saturating_sub(last_processed));
        }
    }

    lag_samples.sort_unstable();
    report.ack_lag_p95 = lag_samples[lag_samples.len() * 95 / 100];
    report.ack_lag_max = *lag_samples.last().unwrap();
    report
}

/// The user's clumsy profile: 255ms delay, ~60ms jitter, 15% loss, both ways.
fn clumsy(label: &'static str, backups: u32, link: Link) -> Cfg {
    Cfg {
        label,
        backups,
        delay_ms: 255.0,
        jitter_ms: 60.0,
        loss: 0.15,
        link,
    }
}

#[test]
fn link_sim_clumsy_profiles() {
    let wt = |age_expiry_ms| Link::Datagram {
        age_expiry_ms,
        stall_ms: 510.0,
        healthy_rate: 4.0,
        collapsed_rate: 0.125,
    };
    let ws = |buffered_cap| Link::Stream {
        buffered_cap,
        bandwidth: None,
    };
    // ~96 kbps uplink: a congested shared mobile link. 12 bytes/ms.
    let ws_throttled = |buffered_cap| Link::Stream {
        buffered_cap,
        bandwidth: Some(12.0),
    };

    let cfgs = [
        clumsy("wt age=50 backups=8   ", 8, wt(Some(50.0))),
        clumsy("wt age=50 backups=16  ", 16, wt(Some(50.0))),
        clumsy("wt age=100 backups=16 ", 16, wt(Some(100.0))),
        clumsy("wt no-expiry backups=16", 16, wt(None)),
        clumsy("ws cap=16k backups=16 ", 16, ws(16 * 1024)),
        clumsy("ws cap=16k backups=1  ", 1, ws(16 * 1024)),
        clumsy("ws cap=inf backups=16 ", 16, ws(usize::MAX)),
        clumsy("ws 96kbps backups=16  ", 16, ws_throttled(16 * 1024)),
        clumsy("ws 96kbps backups=4   ", 4, ws_throttled(16 * 1024)),
        clumsy("ws 96kbps backups=1   ", 1, ws_throttled(16 * 1024)),
    ];

    println!(
        "{:<24} {:>7} {:>7} {:>8} {:>8} {:>8} {:>7} {:>7} {:>9} {:>8}",
        "scenario", "skipped", "max_gap", "lag_p95", "lag_max", "expired", "door", "lost", "delivered", "starved"
    );
    let mut results = Vec::new();
    for cfg in cfgs {
        // Average across seeds so assertions aren't tuned to one RNG stream.
        let mut merged = Report::default();
        for seed in [7, 1337, 0xDEAD_BEEF] {
            let report = simulate(cfg, seed);
            merged.skipped += report.skipped;
            merged.max_gap = merged.max_gap.max(report.max_gap);
            merged.ack_lag_p95 = merged.ack_lag_p95.max(report.ack_lag_p95);
            merged.ack_lag_max = merged.ack_lag_max.max(report.ack_lag_max);
            merged.expired += report.expired;
            merged.door_dropped += report.door_dropped;
            merged.wire_lost += report.wire_lost;
            merged.delivered += report.delivered;
            merged.starved_ticks += report.starved_ticks;
        }
        println!(
            "{:<24} {:>7} {:>7} {:>8} {:>8} {:>8} {:>7} {:>7} {:>9} {:>8}",
            cfg.label,
            merged.skipped,
            merged.max_gap,
            merged.ack_lag_p95,
            merged.ack_lag_max,
            merged.expired,
            merged.door_dropped,
            merged.wire_lost,
            merged.delivered,
            merged.starved_ticks
        );
        results.push(merged);
    }

    let [wt8, wt16, wt16_age100, wt_noexpiry, ws16k, ws_min, ws_inf, thr16, thr4, thr1] =
        &results[..]
    else {
        unreachable!()
    };

    // Sanity: everything delivered traffic.
    for report in &results {
        assert!(report.delivered > 0);
    }

    // Starvation extrapolates without double-stepping. Recovery discards old
    // input states, so acknowledgement age remains bounded on viable links.
    for report in [
        wt8,
        wt16,
        wt16_age100,
        ws16k,
        ws_min,
        ws_inf,
        thr16,
        thr4,
        thr1,
    ] {
        assert!(report.ack_lag_max < 64, "ack lag ran away: {}", report.ack_lag_max);
        assert!(report.starved_ticks > 0, "clumsy must exercise extrapolation");
    }
    assert!(
        wt_noexpiry.ack_lag_max > 1_000,
        "an unbounded stale datagram FIFO should remain pathological"
    );

    // On a reliable transport, redundancy is not insurance — it's load.
    // Oversized packets pressure the bufferedAmount door; lean packets do not.
    assert!(
        thr16.door_dropped > 0,
        "oversized redundancy should pressure the door on a throttled uplink"
    );
    assert_eq!(thr1.door_dropped, 0, "lean packets never pressure the door");

    // Last-intent extrapolation keeps acknowledgement age bounded with or
    // without the reliable transport's door cap.
    for (label, report) in [("ws16k", ws16k), ("ws_min", ws_min), ("ws_inf", ws_inf)] {
        assert!(
            report.ack_lag_p95 < 96,
            "{label} ack lag should stay bounded (p95 {})",
            report.ack_lag_p95
        );
    }
}

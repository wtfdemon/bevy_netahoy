//! The delta-coded usercmd packet: every command comes back bit for bit,
//! whatever the floats hold, and real command streams shrink. Run with
//! `cargo nextest run usercmd_wire --no-capture` for the size table.

use bevy::math::{Vec2, Vec3};

use super::encode;
use crate::{
    client::DATAGRAM_USERCMD_BACKUP_SECONDS, protocol::*, server::SERVER_CMD_QUEUE_MAX_COMMANDS,
};

/// Every command in `bytes`.
fn decode(bytes: &[u8]) -> Result<Vec<UserCmd>, super::WireError> {
    super::decode(bytes, usize::MAX)
}

/// Every field as raw bits: `PartialEq` on f32 says NaN != NaN and
/// -0.0 == 0.0, and the wire must keep both apart.
fn bits(command: &UserCmd) -> Vec<u64> {
    let v2 = |v: Vec2| [u64::from(v.x.to_bits()), u64::from(v.y.to_bits())];
    let mut out = vec![u64::from(command.sequence)];
    out.extend(v2(command.movement));
    out.extend(v2(command.look));
    out.push(u64::from(command.buttons.bits()));
    match command.fire {
        Some(fire) => {
            out.push(1);
            out.push(u64::from(fire.frac.to_bits()));
            out.extend(v2(fire.look));
            match fire.origin {
                Some(origin) => {
                    out.push(1);
                    out.extend(origin.to_array().map(|axis| u64::from(axis.to_bits())));
                }
                None => out.push(0),
            }
        }
        None => out.push(0),
    }
    out.push(command.seen_server_tick);
    out.push(u64::from(command.seen_alpha.to_bits()));
    out
}

fn assert_round_trip(commands: &[UserCmd]) -> usize {
    let blob = encode(commands);
    let decoded = decode(&blob).expect("own encoding decodes");
    assert_eq!(decoded.len(), commands.len());
    for (a, b) in commands.iter().zip(&decoded) {
        assert_eq!(bits(a), bits(b), "{a:?} came back as {b:?}");
    }
    // And through serde, the way replicon sends it: a packet keeps its newest commands.
    let packet = UserCmdPacket {
        commands: commands.to_vec(),
    };
    let wire = postcard::to_allocvec(&packet).unwrap();
    let back: UserCmdPacket = postcard::from_bytes(&wire).unwrap();
    let kept = commands
        .len()
        .saturating_sub(SERVER_CMD_QUEUE_MAX_COMMANDS as usize);
    assert_eq!(
        commands[kept..].iter().map(bits).collect::<Vec<_>>(),
        back.commands.iter().map(bits).collect::<Vec<_>>()
    );
    wire.len()
}

/// xorshift64*: no rand dependency, same stream every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn f32(&mut self) -> f32 {
        const SPECIAL: [f32; 12] = [
            0.0,
            -0.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::MAX,
            f32::MIN,
            f32::MIN_POSITIVE,
            f32::EPSILON,
            1.0,
            -1.0,
            core::f32::consts::PI,
        ];
        match self.below(4) {
            0 => SPECIAL[self.below(SPECIAL.len() as u64) as usize],
            // Any bit pattern: NaN payloads, subnormals, huge values.
            1 => f32::from_bits(self.next() as u32),
            _ => (self.next() as i32) as f32 / (1u64 << self.below(40)) as f32,
        }
    }

    /// Mostly the previous value nudged (what a tail looks like), sometimes anything.
    fn near(&mut self, prev: f32) -> f32 {
        match self.below(4) {
            0 => prev,
            1 => self.f32(),
            _ => f32::from_bits(prev.to_bits().wrapping_add(self.below(1 << 20) as u32)),
        }
    }

    fn vec2(&mut self, prev: Vec2) -> Vec2 {
        Vec2::new(self.near(prev.x), self.near(prev.y))
    }

    fn command(&mut self, prev: &UserCmd) -> UserCmd {
        let sequence = match self.below(8) {
            0 => self.next() as u32,
            1 => prev.sequence.wrapping_add(self.below(5) as u32),
            _ => prev.sequence.wrapping_add(1),
        };
        let seen_server_tick = match self.below(8) {
            0 => self.next(),
            1 => prev.seen_server_tick.wrapping_sub(self.below(3)),
            _ => prev.seen_server_tick.wrapping_add(1),
        };
        UserCmd {
            sequence,
            movement: self.vec2(prev.movement),
            look: self.vec2(prev.look),
            buttons: if self.below(4) == 0 {
                Buttons::from_bits_retain(self.next() as u32)
            } else {
                prev.buttons
            },
            fire: (self.below(3) == 0).then(|| SubtickFire {
                frac: self.f32(),
                look: self.vec2(prev.look),
                origin: (self.below(2) == 0).then(|| Vec3::new(self.f32(), self.f32(), self.f32())),
            }),
            seen_server_tick,
            seen_alpha: self.near(prev.seen_alpha),
        }
    }

    fn packet(&mut self, len: usize) -> Vec<UserCmd> {
        let mut prev = UserCmd {
            sequence: self.next() as u32,
            seen_server_tick: self.next(),
            ..UserCmd::default()
        };
        (0..len)
            .map(|_| {
                prev = self.command(&prev);
                prev
            })
            .collect()
    }
}

#[test]
fn empty_and_single() {
    assert_round_trip(&[]);
    assert!(encode(&[]).is_empty());
    assert_round_trip(&[UserCmd::default()]);
    let mut rng = Rng(7);
    for _ in 0..2_000 {
        assert_round_trip(&rng.packet(1));
    }
}

#[test]
fn random_packets_round_trip_bit_exact() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let tail = TickRate::default().ticks(DATAGRAM_USERCMD_BACKUP_SECONDS);
        let len = rng.below(tail + 1) as usize;
        assert_round_trip(&rng.packet(len));
    }
    // Far past any cap: the count is implicit, so the coding never tops out.
    for len in [64, 256, 1024] {
        assert_round_trip(&rng.packet(len));
    }
}

/// However long a blob, a received packet keeps its newest 50 commands, and the server queues
/// those: a 1 MiB blob of zeros is over a million commands, a byte each.
#[test]
fn a_packet_keeps_its_newest_commands_however_long() {
    let mut rng = Rng(11);
    let blob = encode(&rng.packet(200));
    let all: Vec<_> = decode(&blob).unwrap().iter().map(bits).collect();
    let newest: Vec<_> = super::decode(&blob, 50).unwrap().iter().map(bits).collect();
    assert_eq!(newest, all[150..]);

    let len = 1 << 20;
    let mut wire = Vec::new();
    super::put_varint(&mut wire, len);
    wire.resize(wire.len() + len as usize, 0);
    let packet: UserCmdPacket = postcard::from_bytes(&wire).unwrap();
    let newest = len as u32;
    let kept = Vec::from_iter(newest - SERVER_CMD_QUEUE_MAX_COMMANDS as u32 + 1..=newest);
    let sequence = |command: &UserCmd| command.sequence;
    assert_eq!(
        packet.commands.iter().map(sequence).collect::<Vec<_>>(),
        kept
    );

    let mut queued = crate::server::QueuedUserCmds::default();
    queued.push_packet(&packet, 0, TickRate::default());
    assert_eq!(
        queued.commands.iter().map(sequence).collect::<Vec<_>>(),
        kept
    );
}

#[test]
fn extremes_round_trip() {
    let all = |f: f32, sequence: u32, tick: u64| UserCmd {
        sequence,
        movement: Vec2::splat(f),
        look: Vec2::new(f, -f),
        buttons: Buttons::from_bits_retain(u32::MAX),
        fire: Some(SubtickFire {
            frac: f,
            look: Vec2::splat(-f),
            origin: Some(Vec3::splat(f)),
        }),
        seen_server_tick: tick,
        seen_alpha: f,
    };
    let floats = [
        0.0,
        -0.0,
        f32::NAN,
        -f32::NAN,
        f32::from_bits(0x7fc0_dead),
        f32::from_bits(0xffff_ffff),
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MAX,
        f32::MIN,
        f32::from_bits(1),
        f32::from_bits(0x8000_0001),
    ];
    let commands: Vec<_> = floats
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            all(
                f,
                (u32::MAX - 5).wrapping_add(i as u32), // wraps through 0
                if i % 2 == 0 {
                    u64::MAX - i as u64
                } else {
                    i as u64
                },
            )
        })
        .collect();
    assert_round_trip(&commands);
    for pair in commands.windows(2) {
        assert_round_trip(&[pair[1], pair[0]]);
    }
}

#[test]
fn malformed_blobs_error_never_panic() {
    let mut rng = Rng(42);
    for _ in 0..2_000 {
        let len = 1 + rng.below(16) as usize;
        let blob = encode(&rng.packet(len));
        for end in 0..blob.len() {
            let _ = decode(&blob[..end]);
        }
        let garbage: Vec<u8> = (0..rng.below(64)).map(|_| rng.next() as u8).collect();
        let _ = decode(&garbage);
    }
    // A varint that never ends, and one past u32 where a u32 goes.
    assert!(
        decode(&[
            0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
        ])
        .is_err()
    );
    assert!(decode(&[0x01, 0xff, 0xff, 0xff, 0xff, 0x7f]).is_err());
    assert!(decode(&[0x20, 0x00]).is_err());
    // A flag the encoder never sets.
    assert!(decode(&[0x40]).is_err());
}

#[test]
fn json_round_trip() {
    let mut rng = Rng(3);
    let packet = UserCmdPacket {
        commands: rng.packet(8),
    };
    let json = serde_json::to_string(&packet).unwrap();
    let back: UserCmdPacket = serde_json::from_str(&json).unwrap();
    assert_eq!(
        packet.commands.iter().map(bits).collect::<Vec<_>>(),
        back.commands.iter().map(bits).collect::<Vec<_>>()
    );
}

/// Real commands from a bot on a harsh link, as the server received them
/// (look, movement, rockets, the seen clock).
fn recorded() -> Vec<UserCmd> {
    let text = include_str!("../../tests/fixtures/usercmds_autopilot_harsh.txt");
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let v: Vec<i64> = line.split(' ').map(|x| x.parse().unwrap()).collect();
            let f = |i: usize| f32::from_bits(v[i] as u32);
            UserCmd {
                sequence: v[0] as u32,
                movement: Vec2::new(f(1), f(2)),
                look: Vec2::new(f(3), f(4)),
                buttons: Buttons::from_bits_retain(v[5] as u32),
                fire: (v[6] >= 0).then(|| SubtickFire {
                    frac: f(6),
                    look: Vec2::new(f(7), f(8)),
                    origin: None,
                }),
                seen_server_tick: v[9] as u64,
                seen_alpha: f(10),
            }
        })
        .collect()
}

#[derive(Default)]
struct Sizes(Vec<usize>);

impl Sizes {
    fn row(&mut self) -> String {
        self.0.sort_unstable();
        let n = self.0.len();
        let mean = self.0.iter().sum::<usize>() as f64 / n as f64;
        format!(
            "mean {mean:6.1}  p50 {:4}  p99 {:4}  max {:4}",
            self.0[n / 2],
            self.0[n * 99 / 100],
            self.0[n - 1]
        )
    }

    fn mean(&self) -> f64 {
        self.0.iter().sum::<usize>() as f64 / self.0.len() as f64
    }
}

/// Plain postcard of the derive: a length, then each command.
fn postcard_len(commands: &[UserCmd]) -> usize {
    postcard::to_allocvec(&commands.to_vec()).unwrap().len()
}

#[test]
fn recorded_stream_sizes() {
    let commands = recorded();
    assert!(commands.len() > 2_000);
    let mut means = Vec::new();
    println!(
        "usercmd packet payload, every tail window of {} recorded commands:",
        commands.len()
    );
    for tail in [1, 2, 4, 6, 8, 10, 12, 16] {
        let (mut old, mut new) = (Sizes::default(), Sizes::default());
        for end in 1..=commands.len() {
            let window = &commands[end.saturating_sub(tail)..end];
            new.0.push(assert_round_trip(window));
            old.0.push(postcard_len(window));
        }
        println!(
            "  tail {tail:2}  postcard {}  |  delta {}",
            old.row(),
            new.row()
        );
        means.push((tail, old.mean(), new.mean()));
    }
    let mean = |tail: usize, delta: bool| {
        let (_, old, new) = means.iter().find(|row| row.0 == tail).unwrap();
        if delta { *new } else { *old }
    };
    // The point of it: a blackout-deep tail for a short tail's bytes.
    assert!(
        mean(10, true) < mean(4, false),
        "tail 10 delta should undercut tail 4 postcard"
    );
    assert!(mean(16, true) < mean(16, false) / 2.5);
}

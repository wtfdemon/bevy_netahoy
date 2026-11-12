//! The wire form of [`UserCmdPacket`](crate::protocol::UserCmdPacket): each command delta-coded
//! against the one before it in the same packet, lossless to the bit.
//!
//! A datagram packet repeats every unacked command (the redundant tail), and
//! neighbouring commands barely differ: the sequence and the seen tick step
//! by one, movement and buttons hold, look and `seen_alpha` move a little.
//!
//! Every packet decodes on its own (the first command is coded against
//! [`UserCmd::default`], never against anything acked), so a lost packet
//! never spoils the next one. Floats travel as their raw bits: the step runs
//! on both peers with the exact same bits, so nothing is quantized.
//!
//! Per command, in packet order (oldest first, as the client queues them):
//!
//! ```text
//! flags u8          which of the fields below follow
//! SEQUENCE_GAP      zigzag varint: sequence - (previous + 1)
//! TICK_GAP          zigzag varint: seen_server_tick - (previous + 1)
//! MOVEMENT          2 x f32 delta
//! LOOK              2 x f32 delta
//! BUTTONS           varint: bits XOR previous bits
//! FIRE              frac raw f32 LE, 2 x f32 delta against this look, then
//!                   u8 0, or u8 1 + origin 3 x raw f32 LE
//! ALPHA             f32 delta
//! ```
//!
//! An f32 delta is the zigzag varint of the wrapping difference of the two
//! `to_bits` patterns: a small turn of the mouse is a small mantissa step,
//! 2-3 bytes. Comparisons are on bits too, so -0.0 and NaN payloads survive.
//! The command count is implicit: commands run to the end of the blob.

use std::collections::VecDeque;

use bevy::math::{Vec2, Vec3};

use crate::protocol::{Buttons, SubtickFire, UserCmd};

const SEQUENCE_GAP: u8 = 1 << 0;
const TICK_GAP: u8 = 1 << 1;
const MOVEMENT: u8 = 1 << 2;
const LOOK: u8 = 1 << 3;
const BUTTONS: u8 = 1 << 4;
const FIRE: u8 = 1 << 5;
/// Never set: a blob with it isn't one [`encode`] wrote.
const UNUSED: u8 = 1 << 6;
const ALPHA: u8 = 1 << 7;

/// A blob that isn't one [`encode`] wrote: cut short, a varint past its
/// type, or a flag it never sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WireError;

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("malformed usercmd packet")
    }
}

pub(crate) fn encode(commands: &[UserCmd]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 * commands.len() + 24);
    let mut prev = UserCmd::default();
    for command in commands {
        let sequence_gap = command.sequence.wrapping_sub(prev.sequence).wrapping_sub(1);
        let tick_gap = command
            .seen_server_tick
            .wrapping_sub(prev.seen_server_tick)
            .wrapping_sub(1);
        let mut flags = 0;
        for (set, flag) in [
            (sequence_gap != 0, SEQUENCE_GAP),
            (tick_gap != 0, TICK_GAP),
            (bits2(command.movement) != bits2(prev.movement), MOVEMENT),
            (bits2(command.look) != bits2(prev.look), LOOK),
            (command.buttons != prev.buttons, BUTTONS),
            (command.fire.is_some(), FIRE),
            (
                command.seen_alpha.to_bits() != prev.seen_alpha.to_bits(),
                ALPHA,
            ),
        ] {
            if set {
                flags |= flag;
            }
        }
        out.push(flags);
        if flags & SEQUENCE_GAP != 0 {
            put_varint(&mut out, zigzag32(sequence_gap).into());
        }
        if flags & TICK_GAP != 0 {
            put_varint(&mut out, zigzag64(tick_gap));
        }
        if flags & MOVEMENT != 0 {
            put_vec2(&mut out, prev.movement, command.movement);
        }
        if flags & LOOK != 0 {
            put_vec2(&mut out, prev.look, command.look);
        }
        if flags & BUTTONS != 0 {
            put_varint(
                &mut out,
                (command.buttons.bits() ^ prev.buttons.bits()).into(),
            );
        }
        if let Some(fire) = command.fire {
            out.extend_from_slice(&fire.frac.to_le_bytes());
            put_vec2(&mut out, command.look, fire.look);
            match fire.origin {
                Some(origin) => {
                    out.push(1);
                    for axis in origin.to_array() {
                        out.extend_from_slice(&axis.to_le_bytes());
                    }
                }
                None => out.push(0),
            }
        }
        if flags & ALPHA != 0 {
            put_f32(&mut out, prev.seen_alpha, command.seen_alpha);
        }
        prev = *command;
    }
    out
}

/// Decodes a blob, keeping its newest `keep` commands. Every command is decoded, each coded
/// against the one before it, but a blob of any length holds at most `keep` in memory.
pub(crate) fn decode(mut bytes: &[u8], keep: usize) -> Result<Vec<UserCmd>, WireError> {
    // Every command is at least its flags byte.
    let mut commands = VecDeque::with_capacity(bytes.len().min(keep).min(64));
    let mut prev = UserCmd::default();
    let bytes = &mut bytes;
    while let Some((&flags, rest)) = bytes.split_first() {
        *bytes = rest;
        if flags & UNUSED != 0 {
            return Err(WireError);
        }
        let mut command = UserCmd {
            sequence: prev.sequence.wrapping_add(1),
            seen_server_tick: prev.seen_server_tick.wrapping_add(1),
            fire: None,
            ..prev
        };
        if flags & SEQUENCE_GAP != 0 {
            let gap = u32::try_from(get_varint(bytes)?).map_err(|_| WireError)?;
            command.sequence = command.sequence.wrapping_add(unzigzag32(gap));
        }
        if flags & TICK_GAP != 0 {
            command.seen_server_tick = command
                .seen_server_tick
                .wrapping_add(unzigzag64(get_varint(bytes)?));
        }
        if flags & MOVEMENT != 0 {
            command.movement = get_vec2(bytes, prev.movement)?;
        }
        if flags & LOOK != 0 {
            command.look = get_vec2(bytes, prev.look)?;
        }
        if flags & BUTTONS != 0 {
            let toggled = u32::try_from(get_varint(bytes)?).map_err(|_| WireError)?;
            command.buttons = Buttons::from_bits_retain(prev.buttons.bits() ^ toggled);
        }
        if flags & FIRE != 0 {
            let (frac, rest) = bytes.split_first_chunk::<4>().ok_or(WireError)?;
            *bytes = rest;
            let frac = f32::from_le_bytes(*frac);
            let look = get_vec2(bytes, command.look)?;
            let (&has_origin, rest) = bytes.split_first().ok_or(WireError)?;
            *bytes = rest;
            let origin = match has_origin {
                0 => None,
                1 => {
                    let (origin, rest) = bytes.split_first_chunk::<12>().ok_or(WireError)?;
                    *bytes = rest;
                    let axis = |at: usize| {
                        f32::from_le_bytes([
                            origin[at],
                            origin[at + 1],
                            origin[at + 2],
                            origin[at + 3],
                        ])
                    };
                    Some(Vec3::new(axis(0), axis(4), axis(8)))
                }
                _ => return Err(WireError),
            };
            command.fire = Some(SubtickFire { frac, look, origin });
        }
        if flags & ALPHA != 0 {
            command.seen_alpha = get_f32(bytes, prev.seen_alpha)?;
        }
        commands.push_back(command);
        if commands.len() > keep {
            commands.pop_front();
        }
        prev = command;
    }
    Ok(commands.into())
}

fn bits2(v: Vec2) -> [u32; 2] {
    [v.x.to_bits(), v.y.to_bits()]
}

fn zigzag32(v: u32) -> u32 {
    let v = v as i32;
    ((v << 1) ^ (v >> 31)) as u32
}

fn unzigzag32(z: u32) -> u32 {
    (z >> 1) ^ (z & 1).wrapping_neg()
}

fn zigzag64(v: u64) -> u64 {
    let v = v as i64;
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag64(z: u64) -> u64 {
    (z >> 1) ^ (z & 1).wrapping_neg()
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &mut &[u8]) -> Result<u64, WireError> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or(WireError)?;
        *bytes = rest;
        let low = u64::from(byte & 0x7f);
        // The tenth byte holds bit 63 only.
        if shift == 63 && low > 1 {
            return Err(WireError);
        }
        v |= low << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(WireError)
}

fn put_f32(out: &mut Vec<u8>, prev: f32, value: f32) {
    put_varint(
        out,
        zigzag32(value.to_bits().wrapping_sub(prev.to_bits())).into(),
    );
}

fn get_f32(bytes: &mut &[u8], prev: f32) -> Result<f32, WireError> {
    let step = u32::try_from(get_varint(bytes)?).map_err(|_| WireError)?;
    Ok(f32::from_bits(
        prev.to_bits().wrapping_add(unzigzag32(step)),
    ))
}

fn put_vec2(out: &mut Vec<u8>, prev: Vec2, value: Vec2) {
    put_f32(out, prev.x, value.x);
    put_f32(out, prev.y, value.y);
}

fn get_vec2(bytes: &mut &[u8], prev: Vec2) -> Result<Vec2, WireError> {
    Ok(Vec2::new(get_f32(bytes, prev.x)?, get_f32(bytes, prev.y)?))
}

#[cfg(test)]
mod tests;

//! The two bots the server plays like players. Every tick it writes each one a [`UserCmd`]
//! (stick, look, buttons) into its [`QueuedUserCmds`] before the commands are applied, so a bot
//! moves by the same step, is replicated the same, and is hit through the same lag compensation
//! as a client's player. Nothing about them is scripted past their input: a rocket that knocks
//! one off its line knocks it off, and it finds its way back.
//!
//! - [`Strafer`]: forward and jump held, the look weaving, so it bunny hops and air strafes
//!   (carves) round a rounded triangle around the level, fast. The pistol's target for subtick
//!   hits: at its speed, the ticks either side of a hit are a metre apart.
//! - [`AdStrafer`]: on the ground in the range in front of the spawn, holding A, then D, then A,
//!   for uneven stretches: target practice.
//!
//! Each brain is a plain struct driven from its player's own state, with no randomness, so a run
//! plays the same every time and `tests/bots.rs` can check them through the real step.

use std::f32::consts::{PI, TAU};

use bevy::prelude::*;
use bevy_ahoy::moves::MoveState;
use bevy_netahoy::*;

use crate::{
    Arsenal, EQUIP_PISTOL,
    shared::{player_collision_layers, player_controller},
};

/// The strafer's player id.
pub const STRAFER_ID: PlayerId = PlayerId(9_001);
/// The A/D strafer's player id.
pub const AD_STRAFER_ID: PlayerId = PlayerId(9_002);

/// What a client calls a player: the bots by their brain, everyone else by number.
pub fn player_name(id: PlayerId) -> String {
    match id {
        STRAFER_ID => "strafer".into(),
        AD_STRAFER_ID => "a/d bot".into(),
        PlayerId(n) => format!("player {n}"),
    }
}

/// Spawns the bots on the server and writes their commands every tick.
pub struct BotsPlugin;

impl Plugin for BotsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_bots).add_systems(
            FixedPreUpdate,
            drive_bots.before(NetAhoyServerSystems::ApplyCommands),
        );
    }
}

/// One tick of a bot's input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Intent {
    /// The stick: x right, y forward, as [`UserCmd::movement`].
    pub movement: Vec2,
    /// The look's yaw (rad): forward is -Z turned by it about +Y.
    pub yaw: f32,
    pub jump: bool,
}

impl Intent {
    /// The command this intent sends as `sequence`, seeing the world as it is at `tick`: a bot
    /// has no lag.
    pub fn command(self, sequence: u32, tick: u64) -> UserCmd {
        let mut buttons = EQUIP_PISTOL;
        buttons.set(Buttons::JUMP, self.jump);
        UserCmd {
            sequence,
            movement: self.movement,
            look: Vec2::new(self.yaw, 0.0),
            buttons,
            fire: None,
            seen_server_tick: tick,
            seen_alpha: 0.0,
        }
    }
}

/// A bot on the server: its brain and the sequence of its last command.
#[derive(Component)]
struct Bot {
    brain: Brain,
    sequence: u32,
}

enum Brain {
    Strafer(Strafer),
    AdStrafer(AdStrafer),
}

fn spawn_bots(mut commands: Commands, shape: Res<PlayerShape>) {
    let strafer = Strafer::default();
    let ad = AdStrafer::default();
    let bots = [
        (STRAFER_ID, strafer.spawn_point(), Brain::Strafer(strafer)),
        (AD_STRAFER_ID, AdStrafer::SPAWN, Brain::AdStrafer(ad)),
    ];
    for (id, position, brain) in bots {
        commands.spawn((
            Name::new(player_name(id)),
            id,
            // Owned by no client: a player with the command queue every player has, whose
            // OwnerSnapshot goes to nobody.
            PlayerOwner(Entity::PLACEHOLDER),
            PlayerState::<Arsenal>::at(position),
            player_controller(),
            shape.collider(),
            player_collision_layers(),
            Bot { brain, sequence: 0 },
        ));
    }
}

fn drive_bots(
    tick: Res<ServerTick>,
    rate: Res<TickRate>,
    mut bots: Query<(&mut Bot, &PlayerState<Arsenal>, &mut QueuedUserCmds)>,
) {
    for (mut bot, state, mut queued) in &mut bots {
        let intent = match &mut bot.brain {
            Brain::Strafer(brain) => brain.think(&state.moves, tick.0, *rate),
            Brain::AdStrafer(brain) => brain.think(&state.moves, tick.0, *rate),
        };
        bot.sequence = bot.sequence.wrapping_add(1);
        queued.push(intent.command(bot.sequence, tick.0));
    }
}

/// Forward and jump held, the look weaving either side of where it heads, round a rounded
/// triangle. Holding forward in the air while the look turns is the controller's air strafe: the
/// velocity follows the look and gains speed for every radian it carves
/// (`CharacterController::carve_gain`), so the weave keeps it fast; holding jump hops again on
/// the tick it lands, and a hop's landing costs no speed. Past [`Self::TOP_SPEED`] it lets go of
/// jump on the next landing and slides on the ground until it is back under
/// [`Self::CRUISE_SPEED`], then hops on: hop, hop, hop, slide.
///
/// It steers by pursuit: it heads for the point [`Self::LOOKAHEAD`] metres on along the circuit
/// from its own angle round the centre, so it closes on the line from anywhere a blast puts it.
#[derive(Clone, Copy, Debug)]
pub struct Strafer {
    /// The look's yaw last tick: the look turns smoothly from here.
    yaw: f32,
    /// Sliding off speed after a landing rather than hopping on.
    braking: bool,
}

impl Default for Strafer {
    fn default() -> Self {
        let spawn = Self::circuit_point(0.0);
        let ahead = Self::circuit_point(0.1);
        Self {
            yaw: yaw_toward(ahead - spawn),
            braking: false,
        }
    }
}

impl Strafer {
    /// The circuit's centre, on the floor.
    pub const CENTER: Vec3 = Vec3::new(0.0, 0.0, -6.0);
    /// The circuit's mean radius (m).
    pub const RADIUS: f32 = 30.0;
    /// How far the radius swings toward the triangle's corners and away at its sides, as a share
    /// of [`Self::RADIUS`].
    pub const ROUNDNESS: f32 = 0.11;
    /// How far ahead along the circuit it steers for (m).
    pub const LOOKAHEAD: f32 = 9.0;
    /// How far the look swings either side of the heading (rad).
    pub const WEAVE: f32 = 0.3;
    /// A full weave, left and back (s): a strafe each way.
    pub const WEAVE_PERIOD: f64 = 1.2;
    /// Lets go of jump on landing above this speed (m/s).
    pub const TOP_SPEED: f32 = 18.0;
    /// Hops again once under this speed (m/s).
    pub const CRUISE_SPEED: f32 = 14.0;
    /// The fastest its look turns toward the heading, outside the weave (rad/s).
    const TURN_RATE: f32 = 5.0;

    /// The circuit at `angle` round the centre (rad, counterclockwise seen from above, 0 along
    /// +X), at standing height: a circle pulled out toward three corners.
    pub fn circuit_point(angle: f32) -> Vec3 {
        let radius = Self::RADIUS * (1.0 + Self::ROUNDNESS * (3.0 * angle).cos());
        Self::CENTER + Vec3::new(angle.cos() * radius, 0.75, -angle.sin() * radius)
    }

    /// Where it spawns: on the circuit at its first corner.
    pub fn spawn_point(&self) -> Vec3 {
        Self::circuit_point(0.0)
    }

    /// The angle round the centre `position` is at: the inverse of [`Self::circuit_point`]'s.
    pub fn circuit_angle(position: Vec3) -> f32 {
        let offset = position - Self::CENTER;
        (-offset.z).atan2(offset.x)
    }

    /// This tick's input, from where its last step left it.
    pub fn think(&mut self, moves: &MoveState, tick: u64, rate: TickRate) -> Intent {
        let dt = rate.seconds() as f32;
        let grounded = moves.ground.is_some();
        let speed = moves.velocity.xz().length();

        // Pursuit: the point a lookahead on along the circuit, from its own angle.
        let angle = Self::circuit_angle(moves.position);
        let target = Self::circuit_point(angle + Self::LOOKAHEAD / Self::RADIUS);
        let heading = yaw_toward(target - moves.position);

        if grounded {
            if speed > Self::TOP_SPEED {
                self.braking = true;
            } else if speed < Self::CRUISE_SPEED {
                self.braking = false;
            }
        }
        let weave = if grounded {
            0.0
        } else {
            Self::WEAVE * triangle_wave(tick, Self::WEAVE_PERIOD, rate)
        };
        // Straight to the weave in the air; turned toward the heading on the ground, so a hop
        // starts where the slide left off.
        let wanted = wrap(heading + weave);
        self.yaw = if grounded {
            turn_toward(self.yaw, wanted, Self::TURN_RATE * dt)
        } else {
            wanted
        };

        Intent {
            movement: Vec2::Y,
            yaw: self.yaw,
            jump: !self.braking,
        }
    }
}

/// Holds A, then D, then A, on the ground in a lane across the range, facing the spawn: hard
/// strafes for target practice. Each stretch lasts one of [`Self::STRETCHES`] in turn, so the
/// rhythm doesn't settle into one a player can time, and turns early at the lane's ends.
#[derive(Clone, Copy, Debug, Default)]
pub struct AdStrafer {
    /// Which stretch it is on.
    stretch: usize,
    /// The tick the stretch started.
    since: u64,
    /// Holding D (right), not A.
    right: bool,
}

impl AdStrafer {
    /// Where it spawns: the middle of its lane.
    pub const SPAWN: Vec3 = Vec3::new(0.0, 0.75, -14.0);
    /// How far either side of [`Self::SPAWN`] along X it may go (m).
    pub const LANE_HALF_WIDTH: f32 = 5.0;
    /// How long each key is held (s), in turn.
    pub const STRETCHES: [f64; 7] = [0.55, 0.9, 0.35, 0.7, 1.1, 0.45, 0.8];
    /// It faces +Z: the spawn.
    const YAW: f32 = PI;

    /// This tick's input, from where its last step left it.
    pub fn think(&mut self, moves: &MoveState, tick: u64, rate: TickRate) -> Intent {
        let held = tick.saturating_sub(self.since);
        let stretch = rate.ticks(Self::STRETCHES[self.stretch % Self::STRETCHES.len()]);
        // Facing +Z, D (right) moves toward -X.
        let offset = moves.position.x - Self::SPAWN.x;
        let at_end = if self.right {
            offset < -Self::LANE_HALF_WIDTH
        } else {
            offset > Self::LANE_HALF_WIDTH
        };
        if held >= stretch || at_end {
            self.right = !self.right;
            self.stretch += 1;
            self.since = tick;
        }
        Intent {
            movement: Vec2::new(if self.right { 1.0 } else { -1.0 }, 0.0),
            yaw: Self::YAW,
            jump: false,
        }
    }
}

/// A triangle wave over `period_seconds`, from -1 up to 1 and back, at `tick`. Taken from the
/// whole tick, so it is exact at any uptime.
fn triangle_wave(tick: u64, period_seconds: f64, rate: TickRate) -> f32 {
    let period = rate.ticks(period_seconds).max(2);
    let phase = (tick % period) as f32 / period as f32;
    if phase < 0.5 {
        4.0 * phase - 1.0
    } else {
        3.0 - 4.0 * phase
    }
}

/// The look's yaw that faces along `direction` on the ground: a look's forward is -Z turned by
/// its yaw about up.
pub fn yaw_toward(direction: Vec3) -> f32 {
    (-direction.x).atan2(-direction.z)
}

/// `angle` in (-pi, pi].
fn wrap(angle: f32) -> f32 {
    PI - (PI - angle).rem_euclid(TAU)
}

/// `from` turned toward `to` by at most `step` (rad), the short way round.
fn turn_toward(from: f32, to: f32, step: f32) -> f32 {
    wrap(from + wrap(to - from).clamp(-step, step))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_toward_matches_the_look() {
        for direction in [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Z,
            Vec3::NEG_Z,
            Vec3::new(1.0, 0.0, -1.0),
        ] {
            let forward = Quat::from_rotation_y(yaw_toward(direction)) * Vec3::NEG_Z;
            assert!(
                forward.abs_diff_eq(direction.normalize(), 1e-5),
                "{direction}"
            );
        }
    }

    #[test]
    fn turn_toward_takes_the_short_way() {
        assert!((turn_toward(3.0, -3.0, 0.1) - 3.1).abs() < 1e-5);
        assert!((turn_toward(0.0, 0.05, 0.1) - 0.05).abs() < 1e-5);
        assert!(
            (turn_toward(3.1, -3.1, 0.1) + 3.1).abs() < 1e-5,
            "wraps past pi"
        );
    }

    /// The circuit's angle is the inverse of its point's, all the way round.
    #[test]
    fn the_circuit_angle_inverts_the_point() {
        for step in -12..12 {
            let angle = step as f32 * 0.25;
            let back = Strafer::circuit_angle(Strafer::circuit_point(angle));
            assert!(wrap(back - angle).abs() < 1e-4, "{angle}: {back}");
        }
    }

    /// The triangle wave runs -1 to 1 and back once a period.
    #[test]
    fn the_triangle_wave_sweeps_both_ways() {
        let rate = TickRate::new(20);
        let wave: Vec<f32> = (0..24).map(|tick| triangle_wave(tick, 1.2, rate)).collect();
        assert_eq!(wave[0], -1.0);
        assert_eq!(wave[12], 1.0);
        assert!(
            (wave[23] - wave[1]).abs() < 1e-6,
            "symmetric about the peak"
        );
        assert!(wave.windows(2).take(12).all(|pair| pair[1] > pair[0]));
    }
}

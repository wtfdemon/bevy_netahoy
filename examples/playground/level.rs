//! The level both binaries build: a dev-textured test map in the style of a Hammer one. An
//! orange floor and a ring of stations round the spawn, each showing one thing the library does,
//! with the strafer's circuit around them all. Static blocks are the same colliders on both
//! peers; the moving platform is a body the server moves and replicates.
//!
//! Seen from the spawn, which faces -Z:
//!
//! - ahead, the range: the A/D bot strafes in front of a backstop, half of it behind the shield
//!   gate;
//! - left, the surf wedge and two blocks to mantle and crane onto;
//! - right, the rocket jump tower, a wall to tic-tac off, and the moving platform;
//! - all round, beyond them, the strafer's circuit ([`crate::bots::Strafer`]).

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_netahoy::{TickRate, WORLD_COLLISION_LAYER};

/// How a block is painted: the dev textures' colours. The client turns these into materials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    /// The floor and the stations' working faces.
    Orange,
    /// Walls and blocks.
    Grey,
}

/// One static block of the level.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    /// Its entity's name.
    pub name: &'static str,
    /// Its centre.
    pub translation: Vec3,
    /// Its full extents (m).
    pub size: Vec3,
    pub rotation: Quat,
    pub surface: Surface,
}

impl Block {
    const fn new(name: &'static str, translation: Vec3, size: Vec3, surface: Surface) -> Self {
        Self {
            name,
            translation,
            size,
            rotation: Quat::IDENTITY,
            surface,
        }
    }

    fn rotated(self, rotation: Quat) -> Self {
        Self { rotation, ..self }
    }

    /// Its transform.
    pub fn transform(&self) -> Transform {
        Transform {
            translation: self.translation,
            rotation: self.rotation,
            ..default()
        }
    }
}

/// Where the first player spawns, on the spawn pad; the others line up beside it.
pub const SPAWN_POINT: Vec3 = Vec3::new(0.0, 1.4, 9.0);

/// The surf wedge's slopes: steeper than a walkable slope, so a player on one slides, and
/// steering into it while it slides carries the speed along it.
const SURF_SLOPE: f32 = 0.88;
/// The rocket jump tower: too tall to jump or climb (m).
pub const TOWER_HEIGHT: f32 = 6.0;
/// The tower's centre on the floor.
pub const TOWER: Vec3 = Vec3::new(12.0, 0.0, -5.0);
/// The surf wedge's ridge, along Z.
pub const SURF_RIDGE: Vec3 = Vec3::new(-14.0, 0.0, -4.0);
/// The range's backstop: behind the A/D bot, facing the spawn.
pub const BACKSTOP: Vec3 = Vec3::new(0.0, 3.0, -20.0);

/// Every static block: the floor, the stations and the backdrop.
pub fn blocks() -> Vec<Block> {
    use Surface::*;
    let surf_side = |sign: f32| {
        let half_width = 2.6;
        Block::new(
            "surf wedge",
            SURF_RIDGE
                + Vec3::new(
                    sign * half_width * SURF_SLOPE.cos(),
                    half_width * SURF_SLOPE.sin(),
                    0.0,
                ),
            Vec3::new(half_width * 2.0, 0.4, 18.0),
            Orange,
        )
        .rotated(Quat::from_rotation_z(-sign * SURF_SLOPE))
    };
    let mut blocks = vec![
        // Wide enough that a player never runs off it.
        Block::new(
            "floor",
            Vec3::new(0.0, -0.25, 0.0),
            Vec3::new(1000.0, 0.5, 1000.0),
            Orange,
        ),
        Block::new(
            "spawn pad",
            Vec3::new(0.0, 0.25, 9.5),
            Vec3::new(7.0, 0.5, 4.0),
            Grey,
        ),
        // The range.
        Block::new("backstop", BACKSTOP, Vec3::new(18.0, 6.0, 1.0), Grey),
        Block::new(
            "range line",
            Vec3::new(0.0, 0.03, -6.0),
            Vec3::new(14.0, 0.06, 0.3),
            Grey,
        ),
        // The shield gate's posts, either side of its panel.
        Block::new(
            "gate post",
            crate::GATE_CENTER + Vec3::new(-crate::GATE_SIZE.x / 2.0 - 0.2, 0.2, 0.0),
            Vec3::new(0.4, crate::GATE_SIZE.y + 0.4, 0.4),
            Grey,
        ),
        Block::new(
            "gate post",
            crate::GATE_CENTER + Vec3::new(crate::GATE_SIZE.x / 2.0 + 0.2, 0.2, 0.0),
            Vec3::new(0.4, crate::GATE_SIZE.y + 0.4, 0.4),
            Grey,
        ),
        // Left: the surf wedge and the climbing blocks.
        surf_side(-1.0),
        surf_side(1.0),
        // Taller than a jump: run at it, jump and hold Space, and the crane and the mantle pull
        // you over.
        Block::new(
            "crane block",
            Vec3::new(-7.0, 1.2, 4.0),
            Vec3::new(3.0, 2.4, 2.0),
            Grey,
        ),
        Block::new(
            "mantle block",
            Vec3::new(-4.0, 0.8, 1.0),
            Vec3::new(3.0, 1.6, 2.0),
            Grey,
        ),
        // Right: the rocket jump tower, and a wall to tic-tac off (meet it glancing in the air).
        Block::new(
            "rocket jump tower",
            TOWER + Vec3::Y * TOWER_HEIGHT / 2.0,
            Vec3::new(4.0, TOWER_HEIGHT, 4.0),
            Grey,
        ),
        Block::new(
            "tic tac wall",
            Vec3::new(17.0, 3.0, 1.0),
            Vec3::new(0.6, 6.0, 9.0),
            Grey,
        ),
    ];
    // The backdrop: far blocks round the horizon, past the strafer's circuit, hazy in the fog.
    for (index, (angle, distance, width, height)) in [
        (0.2, 140.0, 34.0, 26.0),
        (0.75, 165.0, 46.0, 52.0),
        (1.5, 150.0, 30.0, 34.0),
        (2.3, 175.0, 52.0, 40.0),
        (3.0, 145.0, 36.0, 60.0),
        (3.7, 160.0, 44.0, 30.0),
        (4.4, 185.0, 60.0, 70.0),
        (5.05, 140.0, 28.0, 38.0),
        (5.7, 170.0, 40.0, 28.0),
    ]
    .into_iter()
    .enumerate()
    {
        let at = Vec3::new(f32::cos(angle), 0.0, -f32::sin(angle)) * distance;
        blocks.push(
            Block::new(
                BACKDROP_NAMES[index],
                at + Vec3::Y * height / 2.0,
                Vec3::new(width, height, width),
                Grey,
            )
            .rotated(Quat::from_rotation_y(angle)),
        );
    }
    blocks
}

const BACKDROP_NAMES: [&str; 9] = [
    "backdrop 1",
    "backdrop 2",
    "backdrop 3",
    "backdrop 4",
    "backdrop 5",
    "backdrop 6",
    "backdrop 7",
    "backdrop 8",
    "backdrop 9",
];

/// The level's colliders, on the world layer.
pub fn spawn_colliders(commands: &mut Commands) {
    for block in blocks() {
        commands.spawn((
            Name::new(block.name),
            block.transform(),
            RigidBody::Static,
            Collider::cuboid(block.size.x, block.size.y, block.size.z),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
        ));
    }
}

/// The moving platform's size (m): too tall to step onto, so a player is shoved by its side or
/// jumps on and rides. Each step meets it where that player saw it, in its snapshot history.
pub const PLATFORM_SIZE: Vec3 = Vec3::new(6.0, 1.0, 6.0);

/// The platform at `tick`: right of the spawn, 3 m either side along X, there and back every 8 s.
pub fn platform_position(tick: u64, rate: TickRate) -> Vec3 {
    const CENTER: Vec3 = Vec3::new(9.0, 0.5, 7.0);
    CENTER + Vec3::X * cycle_angle(tick, 8.0, rate).sin() * 3.0
}

/// How far round a cycle of `period_seconds` the server is at `tick`, in radians. The phase is
/// taken from the whole tick, exactly, before any float: an angle from the time since start
/// would lose its low bits as the server stays up, and roughen the path (a body's by 14 mm after
/// 14.5 h in f32).
pub fn cycle_angle(tick: u64, period_seconds: f64, rate: TickRate) -> f32 {
    let period = (period_seconds * f64::from(rate.hz())).round().max(1.0) as u64;
    (tick % period) as f32 / period as f32 * std::f32::consts::TAU
}

/// The platform's collider, on the world layer.
pub fn platform_collider() -> (Collider, CollisionLayers) {
    (
        Collider::cuboid(PLATFORM_SIZE.x, PLATFORM_SIZE.y, PLATFORM_SIZE.z),
        CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
    )
}

/// A sign at a station: a title and a few words on what it shows, flat in the world.
#[derive(Clone, Copy, Debug)]
pub struct Sign {
    pub title: &'static str,
    pub body: &'static str,
    /// Where its title's centre is.
    pub at: Vec3,
    /// Its text runs along local X with up along local Y, readable from local +Z.
    pub rotation: Quat,
    /// The title's height, a metre per em; the text under it is a third of that.
    pub size: f32,
}

/// The stations' signs, in the order a player meets them.
pub fn signs() -> [Sign; 6] {
    use std::f32::consts::FRAC_PI_2;
    let facing = |yaw: f32| Quat::from_rotation_y(yaw);
    // On the floor, read from the spawn.
    let floor = Quat::from_rotation_x(-FRAC_PI_2);
    [
        Sign {
            title: "PREDICTION",
            body: "You move the moment you press: your client\n\
                   runs the server's step itself, and the server's\n\
                   answer only corrects it. F3 shows its copy of you.",
            at: Vec3::new(-6.0, 4.4, 4.0),
            rotation: facing(0.7),
            size: 0.7,
        },
        Sign {
            title: "LAG COMPENSATION",
            body: "Pistol (2) a bot. The server judges the shot where you saw it,\n\
                   between two ticks: a hit leaves its ghost there, and the ticks either side.",
            at: BACKSTOP + Vec3::new(0.0, 1.7, 0.52),
            rotation: facing(0.0),
            size: 1.1,
        },
        Sign {
            title: "SHIELD GATE",
            body: "Game state with history: up, it stops\nshots as you saw it when you fired.",
            at: Vec3::new(crate::GATE_CENTER.x, 0.02, crate::GATE_CENTER.z + 2.2),
            rotation: floor,
            size: 0.8,
        },
        Sign {
            title: "ROCKET JUMP",
            body: "Rockets (1) are predicted, blast and all:\nfire at your feet as you jump.",
            at: TOWER + Vec3::new(0.0, TOWER_HEIGHT - 1.2, 2.03),
            rotation: facing(0.0),
            size: 0.7,
        },
        Sign {
            title: "MOVING PLATFORM",
            body: "A body the server moves: every player\nmeets it where they saw it, and rides it.",
            at: Vec3::new(16.0, 3.4, 7.0),
            rotation: facing(-FRAC_PI_2),
            size: 0.7,
        },
        Sign {
            title: "SURF",
            body: "Slide on the slope, steer into it.\nHold jump to hop off the end.",
            at: SURF_RIDGE + Vec3::new(0.0, 5.4, 9.2),
            rotation: facing(0.0),
            size: 0.8,
        },
    ]
}

//! The step's move as the server and client prediction run it: falls, walks and jumps, riding a
//! body seen from its history, rewind and replay, and the push between players.

// `pub`, so the parts of the harness this file doesn't use aren't dead code.
pub mod common;

use avian3d::prelude::*;
use bevy::ecs::entity::MapEntities;
use bevy::prelude::*;
use bevy_netahoy::{
    BodySnapshot, Buttons, History, NoEvents, PlayerGame, PlayerId, PlayerShape, PlayerState, Step,
    TickRate, UserCmd, WORLD_COLLISION_LAYER, pmove,
    testing::{self, Outbox},
};
use playground::Arsenal;
use serde::{Deserialize, Serialize};

use common::*;

#[test]
fn it_lands_and_walks() {
    let mut app = make_app(Vec3::new(0.0, 2.0, 0.0));
    let landed = *run(&mut app, &idle(1..=20)).last().unwrap();
    assert!(landed.state.moves.ground.is_some(), "landed");
    assert!(
        (landed.position().y - HALF_HEIGHT).abs() < 0.05,
        "on the floor: {}",
        landed.position()
    );

    let walk: Vec<_> = (21..=60)
        .map(|seq| command(seq, Vec2::Y, Buttons::empty()))
        .collect();
    let walked = *run(&mut app, &walk).last().unwrap();
    assert!(
        walked.position().z < -8.0,
        "walked forward (-Z): {}",
        walked.position()
    );
    assert!(walked.state.moves.ground.is_some(), "still on the ground");
}

#[test]
fn a_jump_goes_up_and_lands() {
    let mut app = make_app(Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0));
    run(&mut app, &idle(1..=1));
    let jump: Vec<_> = (2..=3)
        .map(|seq| command(seq, Vec2::ZERO, Buttons::JUMP))
        .collect();
    let up = *run(&mut app, &jump).last().unwrap();
    assert!(
        up.state.moves.ground.is_none() && up.velocity().y > 0.0,
        "jumped"
    );
    let frames = run(&mut app, &idle(4..=40));
    let peak = frames
        .iter()
        .map(|frame| frame.position().y)
        .fold(0.0, f32::max);
    assert!(peak > HALF_HEIGHT + 0.5, "went up: peak {peak}");
    assert!(
        frames.last().unwrap().state.moves.ground.is_some(),
        "landed again"
    );
}

/// Jump pressed for one command and let go two commands before the player lands: the press
/// (set on its command and not on the one before) waits, and the player jumps as it lands.
#[test]
fn a_jump_tapped_just_before_landing_jumps_as_it_lands() {
    let start = Vec3::new(0.0, 3.0, 0.0);
    let falling = run(&mut make_app(start), &idle(1..=30));
    let landing = falling
        .iter()
        .position(|frame| frame.state.moves.ground.is_some())
        .expect("lands");
    assert!(
        landing >= 2,
        "falls for a few commands first: lands on index {landing}"
    );

    let mut commands = idle(1..=30);
    commands[landing - 2].buttons = Buttons::JUMP;
    let frames = run(&mut make_app(start), &commands);
    assert!(
        frames[landing - 2].state.moves.buffered > 0,
        "the press waits"
    );
    let jumped = frames
        .iter()
        .position(|frame| frame.state.moves.since_jump == 0 && frame.velocity().y > 0.0);
    assert_eq!(
        jumped,
        Some(landing + 1),
        "jumps on the command after the one that lands"
    );
}

/// A 4 m square platform, its top at `top`, whose live body never moves: its snapshot history
/// has it at `x(tick)` along X for ticks 1 to 60.
fn spawn_platform(app: &mut App, top: f32, x: impl Fn(u64) -> f32) {
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, top - 0.25, 0.0)),
            RigidBody::Kinematic,
            Collider::cuboid(4.0, 0.5, 4.0),
            CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
            BodySnapshot::default(),
        ))
        .id();
    app.update();
    let mut history = app
        .world_mut()
        .get_mut::<History<BodySnapshot>>(platform)
        .unwrap();
    for tick in 1..=60u64 {
        testing::record(
            &mut history,
            tick,
            BodySnapshot {
                tick,
                position: Vec3::new(x(tick), top - 0.25, 0.0),
                rotation: Quat::IDENTITY,
                ..default()
            },
        );
    }
}

/// A platform whose live body never moves: the step can only meet it where its snapshot
/// history puts it at each command's seen time, and that carries the player standing on it.
#[test]
fn a_platform_from_history_carries_the_player() {
    let top = 1.0;
    let mut app = make_app(Vec3::new(0.0, top + HALF_HEIGHT + 0.01, 0.0));
    // 2 m/s along +X, inside the history's 64 ticks.
    spawn_platform(&mut app, top, |tick| 0.1 * tick as f32);
    // A rider moves with what it rides: from rest it would slide back until friction caught up.
    let mut player = app.world_mut().query::<&mut PlayerState<Arsenal>>();
    player.single_mut(app.world_mut()).unwrap().moves.velocity = Vec3::X * 2.0;

    let frames = run(&mut app, &idle(1..=40));
    let last = frames.last().unwrap().state.moves;
    assert!(last.ground.is_some(), "standing on the platform");
    assert!(
        (last.position.y - (top + HALF_HEIGHT)).abs() < 0.05,
        "on top of it: {}",
        last.position
    );
    assert!(
        (last.position.x - 4.0).abs() < 0.3,
        "carried along with it to x≈4: {}",
        last.position
    );
}

/// A rider on a body moving at constant velocity ends every command at the same offset from
/// the body's pose a whole tick after the command's seen tick: where `mover_times` ends the
/// body, and where a client draws it at the end of the tick. Seen mid-tick (alpha 0.5), as a
/// client's commands are: the move still meets the body at whole ticks.
#[test]
fn a_rider_keeps_its_offset_on_a_mover_at_whole_tick_endpoints() {
    let top = 1.0;
    let mut app = make_app(Vec3::new(0.0, top + HALF_HEIGHT + 0.01, 0.0));
    let body_x = |tick: u64| 0.1 * tick as f32;
    spawn_platform(&mut app, top, body_x);
    let mut player = app.world_mut().query::<&mut PlayerState<Arsenal>>();
    player.single_mut(app.world_mut()).unwrap().moves.velocity = Vec3::X * 2.0;

    let commands: Vec<_> = idle(1..=40)
        .into_iter()
        .map(|command| UserCmd {
            seen_alpha: 0.5,
            ..command
        })
        .collect();
    let frames = run(&mut app, &commands);
    let offsets: Vec<f32> = frames
        .iter()
        .zip(&commands)
        .map(|(frame, command)| frame.position().x - body_x(command.seen_server_tick + 1))
        .collect();
    assert!(
        frames.last().unwrap().state.moves.ground.is_some(),
        "standing on the platform"
    );
    // Settled once it has landed on it.
    let settled = &offsets[5..];
    let (lo, hi) = settled
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    assert!(hi - lo < 1e-3, "the offset holds: {lo}..{hi} ({offsets:?})");
}

/// What reconciliation stands on: rewind to a frame, replay the rest, get every frame again.
#[test]
fn a_rewind_and_replay_reproduces_every_frame() {
    let mut app = make_app(Vec3::new(0.0, 2.0, 0.0));
    let script: Vec<_> = (1..=60)
        .map(|seq| {
            let mut buttons = Buttons::empty();
            if (12..14).contains(&seq) {
                buttons |= Buttons::JUMP;
            }
            if (40..42).contains(&seq) {
                buttons |= Buttons::JUMP;
            }
            command(seq, Vec2::new(0.3, 1.0), buttons)
        })
        .collect();
    let first = run(&mut app, &script);

    for at in [15, 35, 45] {
        let from = first[at];
        rewind(&mut app, from);
        let mut previous = from.command.buttons;
        let again: Vec<_> = script[at + 1..]
            .iter()
            .map(|&command| {
                let frame = run_one(&mut app, &mut Outbox::default(), command, previous);
                previous = command.buttons;
                frame
            })
            .collect();
        for (a, b) in first[at + 1..].iter().zip(&again) {
            assert_eq!(
                a.state, b.state,
                "from {at}, command {}",
                a.command.sequence
            );
        }
    }
}

/// The push between players: a player standing inside another, where its commands saw it, is
/// pushed out along the line between their centres and slides to a stop; dead centre (a spawn
/// stack) it goes out along +X. A player right above, out of reach, changes nothing.
#[test]
fn a_player_inside_another_is_pushed_out_and_one_above_is_not() {
    let start = Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0);
    let script = idle(1..=40);
    let ticks = 50;
    let above = |_| Vec3::new(0.2, HALF_HEIGHT + 1.65, 0.1);
    let alone = run(&mut make_app(start), &script);

    let mut app = make_app(start);
    spawn_seen_player(&mut app, PlayerId(8), ticks, above);
    for (a, b) in alone.iter().zip(&run(&mut app, &script)) {
        assert_eq!(a.state, b.state, "command {}", a.command.sequence);
    }

    let touch = 2.0 * shape().radius;
    let off_axis = Vec3::new(-0.3, HALF_HEIGHT, -0.4);
    for (other, away) in [
        (off_axis, Vec2::new(0.6, 0.8)),
        (start.with_y(HALF_HEIGHT), Vec2::X),
    ] {
        let mut app = make_app(start);
        spawn_seen_player(&mut app, PlayerId(7), ticks, |_| other);
        spawn_seen_player(&mut app, PlayerId(8), ticks, above);
        let frames = run(&mut app, &script);
        let push = frames[0].velocity().xz();
        assert!(
            push.length() > 1.0 && (push.normalize() - away).length() < 1e-4,
            "pushed along {away}: {push}"
        );
        for frame in &frames {
            let off = (frame.position() - other).xz();
            assert!(off.perp_dot(away).abs() < 1e-4, "stays on the axis: {off}");
        }
        let apart: Vec<f32> = frames
            .iter()
            .map(|frame| (frame.position() - other).xz().length())
            .collect();
        let out = apart.iter().position(|&apart| apart >= touch);
        assert!(
            out.is_some_and(|out| out <= 3),
            "out of it in a few commands: {apart:?}"
        );
        let last = frames.last().unwrap();
        assert_eq!(
            last.velocity().xz(),
            Vec2::ZERO,
            "slid to a stop: {apart:?}"
        );
    }
}

/// The push keeps players their [`PlayerShape`] apart: one standing 1 m off is clear of the
/// default shape (0.9 m, two radii) and inside a wider one, which pushes the player away from it.
#[test]
fn the_push_keeps_players_their_shapes_apart() {
    let start = Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0);
    let other = Vec3::new(1.0, HALF_HEIGHT, 0.0);
    let pushed = |shape: PlayerShape| {
        let mut app = make_app(start);
        app.insert_resource(shape);
        spawn_seen_player(&mut app, PlayerId(7), 30, |_| other);
        run(&mut app, &idle(1..=1))[0].velocity().x
    };
    assert_eq!(pushed(shape()), 0.0);
    let wide = PlayerShape {
        radius: 0.6,
        ..shape()
    };
    assert!(pushed(wide) < -1.0, "pushed along -X: {}", pushed(wide));
}

/// The stock move alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
struct Stock;

impl PlayerGame for Stock {
    type Event = NoEvents;
    type Context = ();

    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        step.pmove(&mut player.moves);
    }
}

/// The same move written by the game over [`Step::with_pm`]: [`pmove::wish`] from the command,
/// [`pmove::pmove`] among the movers near the player.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
struct OwnMove;

impl PlayerGame for OwnMove {
    type Event = NoEvents;
    type Context = ();

    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        let wish = pmove::wish(step.command(), step.previous_buttons());
        let moves = &mut player.moves;
        step.with_pm(moves.position, |movers, pm| {
            pmove::pmove(moves, wish, movers, pm)
        });
    }
}

/// A game's own move over [`Step::with_pm`] meets the world the stock move does: written as
/// the stock move, it steps every frame the stock one does, bit for bit, riding a platform seen
/// from its history, walking and jumping off it and landing on the floor; and a rewind and
/// replay of it gets every frame again.
#[test]
fn a_move_written_over_with_pm_steps_and_replays_as_the_stock_one() {
    let top = 1.0;
    let start = Vec3::new(0.0, top + HALF_HEIGHT + 0.01, 0.0);
    let script: Vec<_> = (1..=50)
        .map(|seq| {
            let mut buttons = Buttons::empty();
            buttons.set(Buttons::JUMP, (20..22).contains(&seq));
            let movement = if seq > 15 { Vec2::Y } else { Vec2::ZERO };
            command(seq, movement, buttons)
        })
        .collect();
    let platform = |tick| 0.1 * tick as f32;

    let mut stock = game_app::<Stock>(start, TickRate::default());
    spawn_platform(&mut stock, top, platform);
    let stock = run_game::<Stock>(&mut stock, &script);
    let mut own = game_app::<OwnMove>(start, TickRate::default());
    spawn_platform(&mut own, top, platform);
    let first = run_game::<OwnMove>(&mut own, &script);
    for (a, b) in stock.iter().zip(&first) {
        assert_eq!(
            a.state.moves, b.state.moves,
            "command {}",
            a.command.sequence
        );
    }
    let riding = first[14].state.moves;
    assert!(
        riding.ground.is_some()
            && (riding.position.y - (top + HALF_HEIGHT)).abs() < 0.05
            && riding.position.x > 0.5,
        "carried along on the platform: {}",
        riding.position
    );
    let last = first.last().unwrap().state.moves;
    assert!(
        first[20..]
            .iter()
            .any(|frame| frame.state.moves.ground.is_none())
            && last.ground.is_some()
            && (last.position.y - HALF_HEIGHT).abs() < 0.05,
        "jumped off and stands on the floor: {}",
        last.position
    );

    for at in [10, 25] {
        rewind(&mut own, first[at]);
        let mut previous = first[at].command.buttons;
        for (&command, frame) in script[at + 1..].iter().zip(&first[at + 1..]) {
            let again =
                run_one_game::<OwnMove>(&mut own, &mut Outbox::default(), command, previous);
            previous = command.buttons;
            assert_eq!(
                again.state, frame.state,
                "from {at}, command {}",
                command.sequence
            );
        }
    }
}

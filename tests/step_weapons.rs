//! The example's weapons through the step: the predicted pistol and rockets hit what the command
//! saw, a replay passes each event on once, and fires leave from where the move allows.

// `pub`, so the parts of the harness this file doesn't use aren't dead code.
pub mod common;

use avian3d::prelude::*;
use bevy::{ecs::system::RunSystemOnce, prelude::*};
use bevy_netahoy::{
    Buttons, EventId, PlayerId, StepWorld, SubtickFire, UserCmd, WORLD_COLLISION_LAYER,
    fire_position,
    testing::{Outbox, PredictionFrame},
};
use playground::{Arsenal, EQUIP_PISTOL, Hit, ROCKET_GIRTH, Shot, Weapon, rocket_impulse};

use common::*;

/// A target walking across the player's view 10 m out, at 5 m/s: a quarter metre a tick.
fn walking_target(ticks: f32) -> Vec3 {
    Vec3::new(-5.0 + 0.25 * ticks, HALF_HEIGHT, -10.0)
}

/// The look (yaw, pitch) from `eye` at `at`.
fn look_at(eye: Vec3, at: Vec3) -> Vec2 {
    let d = at - eye;
    Vec2::new((-d.x).atan2(-d.z), d.y.atan2(Vec2::new(d.x, d.z).length()))
}

/// The predicted pistol. A shot at a walking target where the command saw it (its pose history
/// at the command's seen time, metres from its newest pose) hits it; one at where it was half a
/// second earlier misses. A rewind and replay over the shots emits their events again, and the
/// timeline's outbox passes each on once.
#[test]
fn a_pistol_hits_the_seen_pose_and_a_replay_passes_each_event_once() {
    const TARGET: PlayerId = PlayerId(2);
    let start = Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0);
    let mut app = make_app(start);
    spawn_seen_player(&mut app, TARGET, 40, |tick| walking_target(tick as f32));

    let mut outbox = Outbox::default();
    let mut previous = Buttons::empty();
    let mut position = start;
    let mut script = Vec::new();
    let mut frames = Vec::new();
    for seq in 1..=30u32 {
        let mut command = UserCmd {
            seen_alpha: 0.5,
            ..command(seq, Vec2::ZERO, EQUIP_PISTOL)
        };
        let seen = seq as f32 + command.seen_alpha;
        let aim = match seq {
            10 => Some(walking_target(seen)),
            20 => Some(walking_target(seen - 10.0)),
            _ => None,
        };
        if let Some(at) = aim {
            command.look = look_at(position + Vec3::Y * shape().eye_height, at);
            command.fire = Some(SubtickFire {
                frac: 0.5,
                look: command.look,
                origin: None,
            });
        }
        let frame = run_one(&mut app, &mut outbox, command, previous);
        previous = command.buttons;
        position = frame.position();
        script.push(command);
        frames.push(frame);
    }
    let first: Vec<Shot> = written(&mut app);

    let [Shot::Fired(fired), Shot::Hit(hit), Shot::Fired(missed)] = first[..] else {
        panic!("a fire and its hit, then a fire alone: {first:?}");
    };
    let shot = EventId {
        owner: PLAYER,
        sequence: 10,
    };
    assert_eq!((fired.id, fired.weapon), (shot, Weapon::Pistol));
    assert_eq!(
        (hit.id, hit.weapon, hit.direct_hit),
        (shot, Weapon::Pistol, Some(TARGET))
    );
    let seen = walking_target(10.5);
    assert!(
        (hit.point.x - seen.x).abs() < shape().radius + 1e-3,
        "hit where the command saw it, x {}: {}",
        seen.x,
        hit.point
    );
    assert!((fired.start + fired.dir * fired.hit_distance - hit.point).length() < 1e-4);
    assert_eq!((missed.id.sequence, missed.weapon), (20, Weapon::Pistol));

    // Rewind to after command 5 (before both shots) and to after 15 (between them), and replay
    // on the shots' timeline and then on a new one: the replay emits them again either way.
    for (at, rewrites) in [(5, 3), (15, 1)] {
        for (new_timeline, passes) in [(false, 0), (true, rewrites)] {
            rewind(&mut app, frames[at - 1]);
            let mut fresh = Outbox::default();
            let outbox = if new_timeline {
                &mut fresh
            } else {
                &mut outbox
            };
            let mut previous = script[at - 1].buttons;
            for (&command, frame) in script[at..].iter().zip(&frames[at..]) {
                let again = run_one(&mut app, outbox, command, previous);
                previous = command.buttons;
                assert_eq!(
                    again.state, frame.state,
                    "from {at}, command {}",
                    command.sequence
                );
            }
            let passed: Vec<Shot> = written(&mut app);
            assert_eq!(
                passed.len(),
                passes,
                "from {at}, on a new timeline {new_timeline}: {passed:?}"
            );
        }
    }
}

/// The events of every command in `commands` on one timeline, each after the last, and the
/// frame after each.
fn run_shots(app: &mut App, commands: &[UserCmd]) -> Vec<(PredictionFrame<Arsenal>, Vec<Shot>)> {
    let mut outbox = Outbox::default();
    let mut previous = Buttons::empty();
    commands
        .iter()
        .map(|&command| {
            let frame = run_one(app, &mut outbox, command, previous);
            previous = command.buttons;
            (frame, written(app))
        })
        .collect()
}

/// The one hit in `shots`, and the command it was stepped on.
fn only_hit(shots: &[(PredictionFrame<Arsenal>, Vec<Shot>)]) -> (u32, Hit) {
    let hits: Vec<_> = shots
        .iter()
        .flat_map(|(frame, events)| {
            events
                .iter()
                .map(move |event| (frame.command.sequence, event))
        })
        .filter_map(|(sequence, event)| match *event {
            Shot::Hit(hit) => Some((sequence, hit)),
            Shot::Fired(_) => None,
        })
        .collect();
    let [hit] = hits[..] else {
        panic!("one hit: {hits:?}");
    };
    hit
}

/// A rocket at a walking target, aimed where the command saw it: the sweep finds the target on
/// the flight between two commands and the rocket blasts on it, short of the floor its baked
/// path runs into.
#[test]
fn a_rocket_at_a_walking_target_hits_it_directly() {
    const TARGET: PlayerId = PlayerId(2);
    let start = Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0);
    let mut app = make_app(start);
    spawn_seen_player(&mut app, TARGET, 40, |tick| walking_target(tick as f32));
    let eye = run(&mut app, &idle(1..=1))[0].position() + Vec3::Y * shape().eye_height;

    let commands: Vec<_> = (2..=30u32)
        .map(|seq| {
            let idle = command(seq, Vec2::ZERO, Buttons::empty());
            let mut command = UserCmd {
                seen_alpha: 0.5,
                ..idle
            };
            if seq == 10 {
                command.look = look_at(eye, walking_target(seq as f32 + command.seen_alpha));
                command.fire = Some(SubtickFire {
                    frac: 0.5,
                    look: command.look,
                    origin: None,
                });
            }
            command
        })
        .collect();
    let shots = run_shots(&mut app, &commands);
    let fired = shots
        .iter()
        .flat_map(|(_, events)| events)
        .find_map(|event| match *event {
            Shot::Fired(fired) => Some(fired),
            Shot::Hit(_) => None,
        })
        .expect("fired");
    let (stepped, hit) = only_hit(&shots);
    assert_eq!((hit.id, hit.weapon), (fired.id, Weapon::Rockets));
    assert_eq!(hit.direct_hit, Some(TARGET));
    assert!(
        stepped > 10 && stepped == hit.sequence,
        "on a later command: {stepped}"
    );
    let flown = fired.start.distance(hit.point);
    let baked = fired.hit_distance;
    assert!(
        flown < baked - 1.0,
        "short of the floor: {flown} m of {baked} m"
    );
    let seen = walking_target(hit.seen_tick as f32 + hit.seen_alpha);
    let reach = shape().radius + ROCKET_GIRTH;
    assert!(
        (hit.point - seen).xz().length() <= reach + 1e-3,
        "at the target as the command saw it, {seen}: {}",
        hit.point
    );
}

/// A rocket into a wall blasts on the wall's face, no player hit, on the command its baked
/// fuse lands on.
#[test]
fn a_rocket_into_a_wall_blasts_on_its_face() {
    let mut app = make_app(Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0));
    spawn_wall(&mut app, 20.0);
    let mut commands = idle(1..=20);
    commands[2].fire = Some(SubtickFire {
        frac: 0.5,
        look: Vec2::ZERO,
        origin: None,
    });
    let shots = run_shots(&mut app, &commands);
    let (stepped, hit) = only_hit(&shots);
    assert_eq!(
        (hit.id.sequence, hit.weapon, hit.direct_hit),
        (3, Weapon::Rockets, None)
    );
    assert!(
        (hit.point.z + 20.0).abs() < 1e-3,
        "on the face: {}",
        hit.point
    );
    // 20 m at 84 m/s is 4.76 ticks; the click was half a tick into the window.
    assert_eq!(stepped, 3 + 4);
}

/// A rocket fired at the floor a tick into a jump blasts at once, point-blank, and its blast
/// pushes the shooter up by [`rocket_impulse`] in the same step: the rocket jump goes higher
/// than the jump.
#[test]
fn a_rocket_at_the_shooters_feet_jumps_it() {
    let start = Vec3::new(0.0, HALF_HEIGHT + 0.01, 0.0);
    let down = Vec2::new(0.0, -1.5);
    let jump: Vec<_> = (1..=40u32)
        .map(|seq| {
            let mut buttons = Buttons::empty();
            buttons.set(Buttons::JUMP, (2..=4).contains(&seq));
            UserCmd {
                look: down,
                ..command(seq, Vec2::ZERO, buttons)
            }
        })
        .collect();
    let mut rocket_jump = jump.clone();
    rocket_jump[2].fire = Some(SubtickFire {
        frac: 0.0,
        look: down,
        origin: None,
    });

    let jumped = run(&mut make_app(start), &jump);
    let shots = run_shots(&mut make_app(start), &rocket_jump);
    let (stepped, hit) = only_hit(&shots);
    assert_eq!((stepped, hit.id.sequence, hit.direct_hit), (3, 3, None));
    assert!(hit.point.y.abs() < 1e-3, "on the floor: {}", hit.point);

    let (with, without) = (&shots[2].0, &jumped[2]);
    assert!(
        without.state.moves.ground.is_none(),
        "in the air when it fires"
    );
    assert_eq!(
        with.position(),
        without.position(),
        "the blast comes after the move"
    );
    let impulse = rocket_impulse(hit.point, with.position());
    assert!(impulse.y > 10.0, "a real push up: {impulse}");
    assert!(
        (with.velocity() - without.velocity() - impulse).length() < 1e-4,
        "the blast's impulse: {} against {}",
        with.velocity() - without.velocity(),
        without.velocity()
    );
    let high = shots
        .iter()
        .map(|(frame, _)| frame.position().y)
        .fold(0.0, f32::max);
    let low = jumped
        .iter()
        .map(|frame| frame.position().y)
        .fold(0.0, f32::max);
    assert!(
        high > low + 1.0,
        "higher than the jump: {high} m against {low} m"
    );
}

/// [`fire_position`] over the test level's real world.
fn fired_from(app: &mut App, origin: Option<Vec3>, from: Vec3, to: Vec3) -> Vec3 {
    let fire = SubtickFire {
        frac: 0.5,
        look: Vec2::ZERO,
        origin,
    };
    app.world_mut()
        .run_system_once(move |spatial: SpatialQuery| {
            fire_position(
                &fire,
                from,
                to,
                shape().eye_height,
                &StepWorld::live(&spatial),
            )
        })
        .unwrap()
}

#[test]
fn a_fire_with_no_origin_leaves_from_the_move() {
    let mut app = make_app(Vec3::new(0.0, HALF_HEIGHT, 0.0));
    let (from, to) = (Vec3::new(0.0, 1.0, 0.0), Vec3::new(1.0, 1.0, 0.0));
    assert_eq!(
        fired_from(&mut app, None, from, to),
        Vec3::new(0.5, 1.0, 0.0)
    );
}

#[test]
fn a_fire_leaves_from_the_body_it_was_drawn_at() {
    let mut app = make_app(Vec3::new(0.0, HALF_HEIGHT, 0.0));
    let (from, to) = (Vec3::new(0.0, 1.0, 0.0), Vec3::new(1.0, 1.0, 0.0));
    // A tick behind at 20 m/s, as a drawn body is.
    let drawn = Vec3::new(-0.5, 1.0, 0.0);
    assert_eq!(fired_from(&mut app, Some(drawn), from, to), drawn);
}

#[test]
fn a_fire_reaches_no_further_than_the_move_allows() {
    let mut app = make_app(Vec3::new(0.0, HALF_HEIGHT, 0.0));
    // Standing still: the slack alone.
    let still = Vec3::new(0.0, 1.0, 0.0);
    let claimed = Vec3::new(0.0, 1.0, 9.0);
    let fired = fired_from(&mut app, Some(claimed), still, still);
    assert!(
        (fired - Vec3::new(0.0, 1.0, 1.0)).length() < 1e-5,
        "{fired}"
    );
    // A respawn's whole-map move doesn't open the reach up.
    let (from, to) = (Vec3::new(-50.0, 1.0, 0.0), Vec3::new(50.0, 1.0, 0.0));
    let fired = fired_from(&mut app, Some(Vec3::new(0.0, 1.0, 90.0)), from, to);
    assert!(
        (fired - Vec3::new(0.0, 1.0, 4.0)).length() < 1e-5,
        "{fired}"
    );
}

#[test]
fn a_fire_cant_reach_through_a_wall() {
    let mut app = make_app(Vec3::new(0.0, HALF_HEIGHT, 0.0));
    // A wall across +X, 3 m out.
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(3.0, 2.0, 0.0)),
        RigidBody::Static,
        Collider::cuboid(1.0, 4.0, 10.0),
        CollisionLayers::new(WORLD_COLLISION_LAYER, LayerMask::ALL),
    ));
    for _ in 0..3 {
        app.update();
    }
    // The wall's near face is at x = 2.5, this origin is within reach but past it.
    let from = Vec3::new(2.0, 1.0, 0.0);
    let past_it = Vec3::new(3.0, 1.0, 0.0);
    assert_eq!(fired_from(&mut app, Some(past_it), from, from), from);
    // Beside the body on the near side, the wall doesn't matter.
    let beside = Vec3::new(2.0, 1.0, 0.5);
    assert_eq!(fired_from(&mut app, Some(beside), from, from), beside);
}

//! The example's shield gate through the step: a shot meets the gate as its command saw it, in
//! the gate's history at the seen time, whatever the gate did since; a replay judges it the same.

// `pub`, so the parts of the harness this file doesn't use aren't dead code.
pub mod common;

use bevy::prelude::*;
use bevy_netahoy::{
    Buttons, History, PlayerId, SubtickFire, UserCmd,
    testing::{self, Outbox, PredictionFrame},
};
use playground::{Arsenal, EQUIP_PISTOL, GATE_CENTER, ShieldGate, Shot};

use common::*;

const TARGET: PlayerId = PlayerId(2);
/// The tick the server raised the gate on: down before it, up from it on.
const RAISED: u64 = 20;
/// The newest tick the histories hold: the server raised the gate long before the shots step.
const NEWEST: u64 = 40;

/// The player 3 m in front of the gate (the spawn's side), the target 3 m behind it, both
/// standing still;
/// the gate's history down until [`RAISED`], up after it, through [`NEWEST`].
fn gate_app() -> App {
    let start = Vec3::new(GATE_CENTER.x, HALF_HEIGHT + 0.01, GATE_CENTER.z + 3.0);
    let mut app = make_app(start);
    spawn_seen_player(&mut app, TARGET, NEWEST, |_| target());
    let world = app.world_mut();
    let gate = world.spawn(History::<ShieldGate>::default()).id();
    let mut history = world.get_mut::<History<ShieldGate>>(gate).unwrap();
    for tick in 1..=NEWEST {
        let raised = tick >= RAISED;
        testing::record(&mut history, tick, ShieldGate { raised });
    }
    app
}

fn target() -> Vec3 {
    Vec3::new(GATE_CENTER.x, HALF_HEIGHT, GATE_CENTER.z - 3.0)
}

/// Commands 1 to `last`, holding `weapon`, each seeing its own tick; a fire at the target on
/// each of `fires`.
fn script(last: u32, weapon: Buttons, fires: &[u32]) -> Vec<UserCmd> {
    let eye = Vec3::new(GATE_CENTER.x, HALF_HEIGHT + 0.01, GATE_CENTER.z + 3.0)
        + Vec3::Y * shape().eye_height;
    let to = target() - eye;
    let look = Vec2::new(
        (-to.x).atan2(-to.z),
        to.y.atan2(Vec2::new(to.x, to.z).length()),
    );
    (1..=last)
        .map(|sequence| UserCmd {
            look,
            fire: fires.contains(&sequence).then_some(SubtickFire {
                frac: 0.5,
                look,
                origin: None,
            }),
            ..command(sequence, Vec2::ZERO, weapon)
        })
        .collect()
}

/// Steps `commands` on one timeline: every frame and every event.
fn step_all(app: &mut App, commands: &[UserCmd]) -> (Vec<PredictionFrame<Arsenal>>, Vec<Shot>) {
    let mut outbox = Outbox::default();
    let mut previous = Buttons::empty();
    let frames = commands
        .iter()
        .map(|&command| {
            let frame = run_one(app, &mut outbox, command, previous);
            previous = command.buttons;
            frame
        })
        .collect();
    (frames, written(app))
}

/// The player after each frame.
fn states(frames: &[PredictionFrame<Arsenal>]) -> Vec<bevy_netahoy::PlayerState<Arsenal>> {
    frames.iter().map(|frame| frame.state).collect()
}

/// Where a bullet stopped: its fire's end.
fn bullet_end(shot: &Shot) -> Option<Vec3> {
    match shot {
        Shot::Fired(fired) => Some(fired.start + fired.dir * fired.hit_distance),
        Shot::Hit(_) => None,
    }
}

/// A bullet fired at a gate the shooter saw down passes and hits, though the gate is up in the
/// server's newest ticks; one fired at a gate seen up stops on it. A rewind and replay of the
/// same commands judges both the same.
#[test]
fn a_bullet_meets_the_gate_as_its_command_saw_it() {
    let mut app = gate_app();
    let commands = script(30, EQUIP_PISTOL, &[10, 25]);
    let (frames, shots) = step_all(&mut app, &commands);

    let [Shot::Fired(_), Shot::Hit(hit), seen_up] = shots[..] else {
        panic!("a fire and its hit, then a fire alone: {shots:?}");
    };
    assert_eq!((hit.id.sequence, hit.direct_hit), (10, Some(TARGET)));
    let end = bullet_end(&seen_up).unwrap();
    assert!(
        (end.z - (GATE_CENTER.z + 0.05)).abs() < 1e-3,
        "the second stops on the gate's face: {end}"
    );

    // From after command 5, before both shots: the same frames, the same events.
    rewind(&mut app, frames[4]);
    let (again, replayed) = step_all(&mut app, &commands[5..]);
    assert_eq!(states(&again), states(&frames[5..]));
    assert_eq!(replayed, shots);
}

/// A rocket is judged per command, at that command's seen time: one that crosses the gate on a
/// command that saw it down flies on and hits the target, even when the gate is up by the time
/// the rocket reaches the target; one that reaches it on a command that saw it up blasts on it.
#[test]
fn a_rocket_meets_the_gate_on_the_command_it_crosses() {
    let mut app = gate_app();
    // 84 m/s at 20 Hz is 4.2 m a command: the rocket fired on 18 crosses the gate (3 m out) on
    // 19, seen down, and reaches the target (6 m out) on 20, seen up. The one fired on 24
    // crosses on 25, seen up.
    let commands = script(30, Buttons::empty(), &[18, 24]);
    let (frames, shots) = step_all(&mut app, &commands);
    let hits: Vec<_> = shots
        .iter()
        .filter_map(|shot| match shot {
            Shot::Hit(hit) => Some(*hit),
            Shot::Fired(_) => None,
        })
        .collect();
    let [through, blocked] = hits[..] else {
        panic!("two blasts: {shots:?}");
    };
    assert_eq!(
        (through.id.sequence, through.sequence, through.direct_hit),
        (18, 20, Some(TARGET))
    );
    assert_eq!(
        (blocked.id.sequence, blocked.sequence, blocked.direct_hit),
        (24, 25, None)
    );
    assert!(
        (blocked.point.z - (GATE_CENTER.z + 0.05)).abs() < 1e-3,
        "blasts on the gate's face: {}",
        blocked.point
    );

    rewind(&mut app, frames[9]);
    let (again, replayed) = step_all(&mut app, &commands[10..]);
    assert_eq!(states(&again), states(&frames[10..]));
    assert_eq!(replayed, shots);
}

//! The server runner and its audit: one fixed tick per frame, so every tick reaches the wire.

use bevy::prelude::*;

use super::ServerTick;

/// Counts frames that ran more than one fixed tick. Replicon sends once per frame, so such a
/// frame replicates only its newest tick, and every client history holds the tick before over
/// the skipped one. Warned at once, summed every 30 s; [`one_tick_per_frame`] keeps it at one.
#[derive(Resource, Default)]
pub(super) struct TicksPerFrame {
    last_tick: u64,
    frames: u32,
    multi: u32,
    max: u64,
    window: f32,
}

pub(super) fn audit_ticks_per_frame(
    tick: Res<ServerTick>,
    real: Res<Time<Real>>,
    mut audit: ResMut<TicksPerFrame>,
) {
    let ticks = tick.0 - core::mem::replace(&mut audit.last_tick, tick.0);
    audit.frames += 1;
    audit.max = audit.max.max(ticks);
    if ticks > 1 {
        audit.multi += 1;
        warn!("frame ran {ticks} fixed ticks: replication merges them, histories lose a tick");
    }
    audit.window += real.delta_secs();
    if audit.window >= 30.0 {
        info!(
            "ticks per frame, last 30s: {} frames, max {} per frame, {} frames with more than one",
            audit.frames, audit.max, audit.multi
        );
        *audit = TicksPerFrame {
            last_tick: tick.0,
            ..default()
        };
    }
}

/// A server runner: exactly one fixed tick per frame, so every tick reaches the wire (replicon
/// sends once per frame).
///
/// Each `update` advances virtual time by exactly one timestep; the loop sleeps until the next
/// tick is due or, behind, runs frames back to back until it catches up. A backlog past a
/// second is dropped (the clock re-anchors) instead of spiralling.
///
/// `app.set_runner(bevy_netahoy::one_tick_per_frame)`, with no winit. Native only: it sleeps
/// the thread.
#[cfg(not(target_family = "wasm"))]
pub fn one_tick_per_frame(mut app: App) -> AppExit {
    use bevy::{app::PluginsState, time::TimeUpdateStrategy};
    use std::time::{Duration, Instant};
    /// Past the boundary, so float rounding never leaves a frame 1 ns short.
    const MARGIN: Duration = Duration::from_micros(200);
    /// How far behind real time the loop catches up rather than dropping the backlog.
    const MAX_BEHIND: Duration = Duration::from_secs(1);
    if app.plugins_state() != PluginsState::Cleaned {
        while app.plugins_state() == PluginsState::Adding {
            bevy::tasks::tick_global_task_pools_on_main_thread();
        }
        app.finish();
        app.cleanup();
    }
    let step = app.world().resource::<Time<Fixed>>().timestep();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    app.world_mut()
        .resource_mut::<Time<Virtual>>()
        .set_max_delta(step);
    let mut anchor = Instant::now();
    let mut ticks: u32 = 0;
    loop {
        app.update();
        if let Some(exit) = app.should_exit() {
            return exit;
        }
        ticks += 1;
        let due = anchor + step * ticks + MARGIN;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > MAX_BEHIND {
            warn!(
                "server fell {:.0} ms behind; dropping the backlog",
                (now - due).as_secs_f64() * 1000.0
            );
            anchor = now;
            ticks = 0;
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use bevy::prelude::*;

    use super::one_tick_per_frame;

    /// A frame that ends more than a second behind drops the backlog: the frames after it run
    /// a tick apart from where it left off, not back to back to catch up.
    #[test]
    fn a_backlog_past_a_second_is_dropped() {
        const STEP: Duration = Duration::from_millis(10);
        const HITCH: Duration = Duration::from_millis(1300);
        const HITCHED: usize = 3;
        const AFTER: usize = 5;

        let starts = Arc::new(Mutex::new(Vec::new()));
        let frames = starts.clone();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(Time::<Fixed>::from_duration(STEP))
            .add_systems(Update, move |mut exit: MessageWriter<AppExit>| {
                let mut starts = frames.lock().unwrap();
                starts.push(Instant::now());
                if starts.len() == HITCHED {
                    std::thread::sleep(HITCH);
                }
                if starts.len() == HITCHED + AFTER {
                    exit.write(AppExit::Success);
                }
            });
        one_tick_per_frame(app);

        let starts = starts.lock().unwrap();
        let after = &starts[HITCHED..];
        let spread = after[AFTER - 1] - after[0];
        assert!(
            spread >= STEP * 3,
            "{AFTER} frames after the hitch took {spread:?}: caught up instead of dropped"
        );
    }
}

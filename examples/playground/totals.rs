//! `rockets: fired N, exploded N` and `pistol: fired N, hit N`, the running totals, each time
//! the wall clock crosses a multiple of 10 s: the same instants on every peer, so
//! scripts/continuity-analyze.py can hold the server's totals against its clients'. The server
//! counts every player's shots, a client its own predicted ones.

use bevy::prelude::*;

use crate::{Shot, Weapon};

/// Logs the running shot totals every 10 s.
pub fn add_shot_totals(app: &mut App) {
    app.add_systems(Update, log_shot_totals);
}

/// Shots fired and shots that landed (a rocket's blast, a bullet's hit), per weapon.
#[derive(Default)]
struct Totals {
    rockets: [u32; 2],
    pistol: [u32; 2],
}

fn log_shot_totals(
    mut events: MessageReader<Shot>,
    real: Res<Time<Real>>,
    mut totals: Local<Totals>,
    mut window: Local<u64>,
) {
    for event in events.read() {
        let (weapon, landed) = match *event {
            Shot::Fired(fired) => (fired.weapon, 0),
            Shot::Hit(hit) => (hit.weapon, 1),
        };
        let counts = match weapon {
            Weapon::Rockets => &mut totals.rockets,
            Weapon::Pistol => &mut totals.pistol,
        };
        counts[landed] += 1;
    }
    let now = ten_second_window(&real);
    if *window != 0 && now != *window {
        let ([fired, exploded], [shot, hit]) = (totals.rockets, totals.pistol);
        info!("rockets: fired {fired}, exploded {exploded}");
        info!("pistol: fired {shot}, hit {hit}");
    }
    *window = now;
}

#[cfg(not(target_family = "wasm"))]
fn ten_second_window(_: &Time<Real>) -> u64 {
    let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since_epoch.map_or(1, |since| since.as_secs() / 10)
}

/// From page load: `SystemTime` panics in the browser, and no script lines its log up.
#[cfg(target_family = "wasm")]
fn ten_second_window(real: &Time<Real>) -> u64 {
    real.elapsed().as_secs() / 10 + 1
}

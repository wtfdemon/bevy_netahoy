//! The playground: a small movement shooter on bevy_netahoy, and the library's tour. This crate
//! is what its two binaries, `playground_server` and `playground_client`, share:
//!
//! - [`game`]: the game half of a player, [`Arsenal`] (rockets and a hitscan pistol), its
//!   [`PlayerGame::step`](bevy_netahoy::PlayerGame::step), its events, and the [`ShieldGate`]
//!   it reads through the step's context;
//! - [`level`]: the map, the moving platform's path and the signs;
//! - [`bots`]: two bots the server plays through commands, as it plays a client;
//! - [`shared`]: the protocol and the player's controller and spawn;
//! - [`totals`]: shot counts the network tests check.
//!
//! Replicated types live here, not in a binary: replicon's protocol hash is built from their type
//! names, which must match on the client and the server.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub mod bots;
pub mod game;
pub mod level;
pub mod shared;
pub mod totals;

pub use game::*;

/// Marks the moving platform; replicated so clients attach its collider and visual.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Platform;

/// The server's input rollback from `NETAHOY_ROLLBACK`: on unless it says `0`, `false`, `off`
/// or `no`, in any case and with any spaces around it.
pub fn input_rollback_setting(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// The speed `--slowmo` without a value runs at.
pub const SLOWMO_FACTOR: f32 = 0.1;

/// The time scale `--slowmo[=factor]` on the command line asks for; real time without it.
pub fn time_scale_from_args() -> bevy_netahoy::debug::DebugTimeScale {
    std::env::args()
        .find_map(|arg| {
            let rest = arg.strip_prefix("--slowmo")?;
            Some(bevy_netahoy::debug::DebugTimeScale::new(
                rest.strip_prefix('=')
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(SLOWMO_FACTOR),
            ))
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::input_rollback_setting;

    #[test]
    fn input_rollback_is_on_unless_switched_off() {
        assert!(input_rollback_setting(None));
        assert!(input_rollback_setting(Some("1")));
        assert!(!input_rollback_setting(Some(" Off ")));
        assert!(!input_rollback_setting(Some("0")));
    }
}

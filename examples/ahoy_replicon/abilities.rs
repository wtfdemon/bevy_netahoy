//! Every player shove runs from here in one fixed order. Order is load-bearing:
//! jump_pad sets vertical speed and rocket_jump adds to it, so this is the truth.

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_netahoy::*;

use crate::jumppad::jump_pad;
use crate::rockets::rocket_jump;

/// The single Shove the game registers. It calls each ability in a fixed order so
/// the client and server always compose them the same way and replay stays exact.
fn shove(
    view: MoveView,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    world: &SpatialQuery,
    pending_shoves: &mut PendingShoves,
    velocity: &mut Vec3,
) {
    jump_pad(view, command, previous_buttons, world, pending_shoves, velocity);
    rocket_jump(view, command, previous_buttons, world, pending_shoves, velocity);
}

pub fn register_shoves(mut shoves: ResMut<Shoves>) {
    shoves.0.push(shove);
}

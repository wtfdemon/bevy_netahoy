//! The stock move. The player is one plain struct, a tick is one plain function over it, and
//! the only way it touches the world is [`Pm::world`], the world as the player saw it at the
//! command's seen time.
//!
//! [`pmove`] runs bevy_ahoy's moves ([`bevy_ahoy::moves`]) in order, and
//! [`Step::pmove`](crate::Step::pmove) runs it on [`wish`] from the command. A game that wants
//! other moves writes its own over [`Step::with_pm`](crate::Step::with_pm).

use bevy::prelude::*;
use bevy_ahoy::{
    Mover,
    moves::{self, MoveState, Pm, Wish},
};

use crate::protocol::{Buttons, UserCmd};

/// What the command asks of the movement, `previous` being the buttons of the command stepped
/// before it. Jump is held: it fires on the tick it lands while held (autohop). A jump press
/// (set now and not on `previous`) that can't act yet waits a few ticks for one it can
/// ([`pmove`]), so letting go just before landing still jumps.
pub fn wish(command: &UserCmd, previous: Buttons) -> Wish {
    let pressed = command.buttons - previous;
    Wish {
        movement: command.movement.clamp_length_max(1.0),
        jump: command.buttons.contains(Buttons::JUMP),
        jump_pressed: pressed.contains(Buttons::JUMP),
        crouch: command.buttons.contains(Buttons::CROUCH),
    }
}

/// The command's look as an orientation: yaw about up, then pitch.
pub fn orientation(command: &UserCmd) -> Quat {
    Quat::from_euler(
        EulerRot::YXZ,
        command.look.x,
        command.look.y.clamp(-1.5, 1.5),
        0.0,
    )
}

/// The orientation's look back out: yaw and pitch.
pub fn look(orientation: Quat) -> Vec2 {
    let (yaw, pitch, _) = orientation.to_euler(EulerRot::YXZ);
    Vec2::new(yaw, pitch)
}

/// Move one player through one command, among `movers` (the same ones `pm.world` holds).
pub fn pmove(s: &mut MoveState, mut wish: Wish, movers: &[Mover], pm: &Pm) {
    moves::buffer_presses(s, &mut wish, pm);
    moves::age(s);

    // The mover under the player carries it first; then it moves itself in the frame that
    // left it in, among the movers standing still at their next pose.
    let ride = moves::ride(s, movers, pm);
    moves::depenetrate(s, pm);
    moves::categorize(s, pm);
    moves::crouch(s, &wish, pm);
    moves::start_gravity(s, pm);

    let wish_velocity = moves::wish_velocity(s, &wish, pm);
    moves::jump(s, &wish, pm);
    // Friction before any base velocity: on a conveyor, standing still doesn't slow you.
    moves::friction(s, pm);
    moves::clamp_velocity(s, pm);
    if s.ground.is_some() {
        moves::ground_move(s, wish_velocity, pm);
    } else {
        moves::air_move(s, wish_velocity, pm);
    }

    moves::categorize(s, pm);
    moves::clamp_velocity(s, pm);
    moves::finish_gravity(s, pm);
    moves::settle(s);
    // Back to the world's frame: whatever the movers gave the player, it keeps.
    moves::unride(s, &ride);
    moves::settle_presses(s);
}

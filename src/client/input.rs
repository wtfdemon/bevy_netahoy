//! What the game writes each frame, and the buttons a command carries.

use bevy::prelude::*;

use crate::{poses::RemoteRenderTime, protocol::*};

/// The local player's input: written by the game every frame, consumed once per fixed tick.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct ClientInput {
    /// The movement stick: x right, y forward, length at most one.
    pub movement: Vec2,
    /// Where the player looks: yaw and pitch, radians.
    pub look: Vec2,
    /// The buttons held, overwritten every frame. Only [`Buttons::JUMP`] and
    /// [`Buttons::CROUCH`] mean anything to the library; the other bits are the game's.
    pub buttons: Buttons,
    /// Press edges, OR-ed in by the game every frame and drained into the next command, so a
    /// tap shorter than a tick still lands. A press the step couldn't see (its bit already set
    /// on the previous command) waits here a tick: no edge is lost.
    pub pressed: Buttons,
    /// The last non-zero `movement`, written by the game every frame and drained into the next
    /// command, which uses it when `movement` is zero: a tap between two ticks still moves.
    pub tap_movement: Vec2,
    /// Latched by the game on a fire click ([`SubtickFire`] carries the click's
    /// sub-tick fraction and exact look); drained into exactly one command.
    pub fire: Option<SubtickFire>,
    /// The frame a pending `fire` was clicked on, latched when the click's frame ran no tick:
    /// the command that carries it is built frames later, and its seen time is this frame's.
    pub(crate) fire_frame: Option<PresentedFrame>,
}

/// What the last rendered frame drew: input sampled in the next frame reacts to it.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentedFrame {
    /// The remote render time remote players were drawn at; `None` before the first.
    pub remote: Option<RemoteRenderTime>,
}

/// A fire the game latched this frame
/// ([`NetAhoyClientSystems::GatherInput`](super::NetAhoyClientSystems::GatherInput))
/// that no tick consumed yet keeps the frame it was clicked on: the last
/// rendered one, still in [`PresentedFrame`] until this frame draws.
pub(super) fn latch_fire_frame(presented: Res<PresentedFrame>, mut input: ResMut<ClientInput>) {
    if input.fire.is_some() && input.fire_frame.is_none() {
        input.fire_frame = Some(*presented);
    }
}

/// A command's buttons from what is held and the presses since the last tick, and the presses
/// to hold back a tick. A step reads a press as a bit set on its command and not on the previous
/// one, so a press of a button the previous command still had (held across that tick, or pressed
/// in the window before it) would never show. Such a press leaves its bit off on this command
/// and sets it on the next: one tick late, and only then.
pub(super) fn command_buttons(
    held: Buttons,
    pressed: Buttons,
    previous: Buttons,
) -> (Buttons, Buttons) {
    let unseen = pressed & previous;
    ((held | pressed) - unseen, unseen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every press reaches the step as an edge: a re-press of a button the last command still
    /// had waits one tick (off, then on), and an ordinary press or hold goes straight through.
    #[test]
    fn every_press_is_an_edge_the_step_sees() {
        let none = Buttons::empty();
        let (jump, crouch) = (Buttons::JUMP, Buttons::CROUCH);
        // A fresh press: on now.
        assert_eq!(command_buttons(none, jump, none), (jump, none));
        // Held on: still on, no edge needed.
        assert_eq!(command_buttons(jump, none, jump), (jump, none));
        // Released and pressed again within one tick, the last command holding it: off this
        // tick, carried, then on the next one, an edge.
        let (now, wait) = command_buttons(jump, jump, jump);
        assert_eq!((now, wait), (none, jump));
        let (next, wait) = command_buttons(jump, wait, now);
        assert_eq!((next, wait), (jump, none));
        assert!((next - now).contains(jump));
        // Two presses in neighbouring windows: the second waits a tick too.
        let (first, _) = command_buttons(none, jump, none);
        let (second, wait) = command_buttons(none, jump, first);
        assert_eq!((second, wait), (none, jump));
        assert_eq!(command_buttons(none, wait, second), (jump, none));
        // Other buttons are untouched by one waiting.
        assert_eq!(command_buttons(crouch | jump, jump, jump), (crouch, jump));
    }
}

//! Hooks for this crate's own tests and harnesses, not API: they step or rewind a player
//! outside the input timelines' bookkeeping, or run one of the plugins' pieces alone.

use bevy::prelude::*;

pub use crate::client::LinkProfile;
pub use crate::events::Outbox;
pub use crate::history::stamp_replication_tick;
pub use crate::protocol::UserCmdPacket;
pub use crate::reconnect::{
    HeldRejoinToken, NetAhoyClientSessionPlugin, NetAhoyServerSessionPlugin,
};
pub use crate::server::OwnerSnapshotPlugin;
pub use crate::stepper::{PredictionFrame, Stepper};
use crate::{Buttons, History, OwnerSnapshot, PlayerGame, Recorded, UserCmd, protocol::InputLead};

/// [`Stepper`]'s step: one command through `entity`'s [`PlayerGame::step`], after the command
/// `previous_sequence` with `previous_buttons`. Its events go through `outbox`, its timeline's:
/// one outbox per command passes every event on, one across commands each once.
pub fn player_move<G: PlayerGame>(
    stepper: &mut Stepper<G>,
    entity: Entity,
    outbox: &mut Outbox<G::Event>,
    command: UserCmd,
    previous_sequence: u32,
    previous_buttons: Buttons,
) -> Result<()> {
    stepper.player_move(entity, outbox, command, previous_sequence, previous_buttons)
}

/// Rewinds `entity` to `snapshot`, as the client does before a replay.
pub fn restore<G: PlayerGame>(
    stepper: &mut Stepper<G>,
    entity: Entity,
    snapshot: &OwnerSnapshot<G>,
) {
    stepper.restore(entity, snapshot);
}

/// The ack of `snapshot`: the newest command whose tick is final.
pub fn ack<G: PlayerGame>(snapshot: &OwnerSnapshot<G>) -> u32 {
    snapshot.last_processed_sequence
}

/// Sets `snapshot`'s ack `sequence`, that command's `buttons` and the owner's `input_lead`, as
/// the server's publish does.
pub fn set_ack<G: PlayerGame>(
    snapshot: &mut OwnerSnapshot<G>,
    sequence: u32,
    buttons: Buttons,
    input_lead: Option<InputLead>,
) {
    snapshot.last_processed_sequence = sequence;
    snapshot.last_processed_buttons = buttons;
    snapshot.input_lead = input_lead;
}

/// Records `value` under `tick` in `history`, as the server's publish or a replication message
/// does. Whether no later tick has a recorded value.
pub fn record<C: Recorded>(history: &mut History<C>, tick: u64, value: C) -> bool {
    history.record(tick, value)
}

/// Holds `history`'s newest value through `tick`, as the client does for ticks it heard
/// nothing about.
pub fn hold_until<C: Recorded>(history: &mut History<C>, tick: u64) {
    history.hold_until(tick);
}

//! The movement context: one user command in, one movement step out.
//! Prediction, replay, and the server all go through [`NetAhoyStepper::player_move`].

use avian3d::prelude::*;
use bevy::{
    ecs::{query::QueryData, schedule::ScheduleLabel, system::SystemParam},
    prelude::*,
    time::Stopwatch,
};
use bevy_ahoy::{CharacterLook, input::AccumulatedInput, prelude::*};

use crate::protocol::{AhoyButtons, AhoySnapshot, AhoyUserCmd, NetAhoyMoveState, PlayerId};
use crate::player::{step_player_state, NetAhoyPlayerState, NetAhoyPlayerEvents};

/// Where Ahoy's own per-tick systems sit. Netcode steps by hand via
/// [`NetAhoyStepper`], so this schedule is never run unless the game runs it
/// itself — pass it to `AhoyPlugins::new` to keep Ahoy out of the fixed loop.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct NetAhoyKccSchedule;

/// Everything the client needs to rewind to (and resimulate from) one
/// predicted command.
#[derive(Clone, Debug)]
pub struct AhoyPredictionFrame {
    pub command: AhoyUserCmd,
    pub position: Vec3,
    pub velocity: Vec3,
    pub look: Vec2,
    pub state: NetAhoyMoveState,
    pub controller_state: CharacterControllerState,
    pub accumulated_input: AccumulatedInput,
    pub player_state: NetAhoyPlayerState,
}

#[derive(QueryData)]
#[query_data(mutable)]
pub struct PmoveParts {
    input: &'static mut AccumulatedInput,
    look: &'static mut CharacterLook,
    transform: &'static mut Transform,
    position: &'static mut Position,
    velocity: &'static mut LinearVelocity,
    state: &'static mut CharacterControllerState,
    player_id: &'static PlayerId,
    player_state: &'static mut NetAhoyPlayerState,
}

/// The movement context: all the Bevy bits a step needs, so callers stay short.
#[derive(SystemParam)]
pub struct NetAhoyStepper<'w, 's> {
    // ParamSet because the Ahoy stepper's internal query also writes
    // Transform/Position/LinearVelocity/AccumulatedInput.
    set: ParamSet<
        'w,
        's,
        (
            CharacterControllerStepper<'w, 's>,
            Query<'w, 's, PmoveParts>,
            SpatialQuery<'w, 's>,
        ),
    >,
    fixed_time: Res<'w, Time<Fixed>>,
    // Server-only outbox for what step_player_state fires/blasts; None on the client,
    // where prediction and replay would push duplicates.
    player_events: Option<ResMut<'w, NetAhoyPlayerEvents>>,
}

impl NetAhoyStepper<'_, '_> {
    /// Run one command through one movement step for one player. The infamous Quake `pmove`
    pub fn player_move(
        &mut self,
        entity: Entity,
        command: AhoyUserCmd,
        previous_buttons: AhoyButtons,
    ) -> Result<()> {
        let fixed_delta = self.fixed_time.timestep();

        {
            let mut players = self.set.p1();
            let mut parts = players.get_mut(entity)?;
            tick_input_timers(&mut parts.input, fixed_delta);
            clear_transient_input(&mut parts.input);
            apply_usercmd(&mut parts.input, &mut parts.look, command, previous_buttons);
        }

        self.set.p0().step_entity(entity, fixed_delta)?;

        // The step writes Transform; Position is what the next step reads.
        let mut players = self.set.p1();
        let mut parts = players.get_mut(entity)?;
        parts.position.0 = parts.transform.translation;

        self.player_think(entity, &command, previous_buttons)
    }

    /// Step the game POD after the KCC step, so client replay and the server
    /// compose identically. `step_player_state` needs [`SpatialQuery`] (p2) alongside
    /// the POD and velocity (p1), which can't be borrowed at once, so we take
    /// the POD out and copy velocity, run it, then write both back.
    fn player_think(
        &mut self,
        entity: Entity,
        command: &AhoyUserCmd,
        previous_buttons: AhoyButtons,
    ) -> Result<()> {
        let (mut player_state, position, look, mut velocity, owner) = {
            let mut players = self.set.p1();
            let parts = players.get_mut(entity)?;
            (
                *parts.player_state,
                parts.transform.translation,
                Vec2::new(parts.look.yaw, parts.look.pitch),
                parts.velocity.0,
                *parts.player_id,
            )
        };

        let spatial = self.set.p2();
        step_player_state(
            &mut player_state,
            self.player_events.as_deref_mut(),
            owner,
            command,
            previous_buttons,
            position,
            look,
            &spatial,
            &mut velocity,
        );

        let mut players = self.set.p1();
        let mut parts = players.get_mut(entity)?;
        parts.velocity.0 = velocity;
        *parts.player_state = player_state;
        Ok(())
    }

    /// Record the post-step state for `command` so it can be restored later.
    pub fn capture_frame(&mut self, entity: Entity, command: AhoyUserCmd) -> Option<AhoyPredictionFrame> {
        let mut players = self.set.p1();
        let parts = players.get_mut(entity).ok()?;
        Some(AhoyPredictionFrame {
            command,
            position: parts.transform.translation,
            velocity: parts.velocity.0,
            look: Vec2::new(parts.look.yaw, parts.look.pitch),
            state: NetAhoyMoveState::from_controller_state(&parts.state),
            controller_state: parts.state.clone(),
            accumulated_input: parts.input.clone(),
            player_state: *parts.player_state,
        })
    }

    /// Rewind `entity` to an authoritative snapshot, reusing the locally
    /// recorded controller/input/state for that tick when available.
    pub fn restore(
        &mut self,
        entity: Entity,
        snapshot: &AhoySnapshot,
        local_state: Option<(&CharacterControllerState, &AccumulatedInput, &NetAhoyPlayerState)>,
    ) {
        let mut players = self.set.p1();
        let Ok(mut parts) = players.get_mut(entity) else {
            return;
        };

        parts.transform.translation = snapshot.position;
        parts.position.0 = snapshot.position;
        parts.velocity.0 = snapshot.velocity;
        parts.look.yaw = snapshot.look.x;
        parts.look.pitch = snapshot.look.y;

        if let Some((stored_state, stored_input, stored_player_state)) = local_state {
            *parts.state = stored_state.clone();
            *parts.input = stored_input.clone();
            *parts.player_state = *stored_player_state;
        } else {
            *parts.state = CharacterControllerState::default();
            *parts.input = AccumulatedInput::default();
            *parts.player_state = NetAhoyPlayerState::default();
        }

        snapshot.state.apply_to_controller_state(&mut parts.state);
        // The net subset: server truth for weapon state stomps whatever the
        // frame (or default) held. The rockets stay from the frame — they're
        // re-derivable from the command stream, so they never ride the wire.
        parts.player_state.weapon = snapshot.weapon;
    }

    pub fn position(&mut self, entity: Entity) -> Option<Vec3> {
        let players = self.set.p1();
        players
            .get(entity)
            .ok()
            .map(|parts| parts.transform.translation)
    }

    /// Drop held movement input without stepping, for ticks with no command.
    pub fn clear_transient(&mut self, entity: Entity) {
        let mut players = self.set.p1();
        if let Ok(mut parts) = players.get_mut(entity) {
            clear_transient_input(&mut parts.input);
        }
    }
}

fn tick_input_timers(input: &mut AccumulatedInput, delta: std::time::Duration) {
    if let Some(timer) = input.jumped.as_mut() {
        timer.tick(delta);
    }
    if let Some(timer) = input.tac.as_mut() {
        timer.tick(delta);
    }
    if let Some(timer) = input.craned.as_mut() {
        timer.tick(delta);
    }
    if let Some(timer) = input.mantled.as_mut() {
        timer.tick(delta);
    }
    if let Some(timer) = input.climbdown.as_mut() {
        timer.tick(delta);
    }
}

fn clear_transient_input(input: &mut AccumulatedInput) {
    input.last_movement = None;
    input.swim_up = false;
    input.crouched = false;
}

fn apply_usercmd(
    input: &mut AccumulatedInput,
    look: &mut CharacterLook,
    command: AhoyUserCmd,
    previous_buttons: AhoyButtons,
) {
    input.last_movement = Some(command.movement.clamp_length_max(1.0));
    input.swim_up = command.buttons.contains(AhoyButtons::SWIM_UP);
    input.crouched = command.buttons.contains(AhoyButtons::CROUCH);

    // Ahoy's normal input observers fire held Jump every frame. Preserve that
    // here so holding Space can auto-bhop through usercmds.
    if command.buttons.contains(AhoyButtons::JUMP) {
        input.jumped = Some(Stopwatch::new());
    }

    // Bits set this command but not last = rising edges.
    let pressed = command.buttons - previous_buttons;
    if pressed.contains(AhoyButtons::TAC) {
        input.tac = Some(Stopwatch::new());
    }
    if pressed.contains(AhoyButtons::CRANE) {
        input.craned = Some(Stopwatch::new());
    }
    if pressed.contains(AhoyButtons::MANTLE) {
        input.mantled = Some(Stopwatch::new());
    }
    if pressed.contains(AhoyButtons::CLIMBDOWN) {
        input.climbdown = Some(Stopwatch::new());
    }

    look.yaw = command.look.x;
    look.pitch = command.look.y.clamp(-1.5, 1.5);
}

//! The movement context: one user command in, one movement step out.
//! Prediction, replay, and the server all go through [`NetAhoyStepper::step`].

use avian3d::prelude::*;
use bevy::{
    ecs::{query::QueryData, schedule::ScheduleLabel, system::SystemParam},
    prelude::*,
    time::Stopwatch,
};
use bevy_ahoy::{CharacterLook, input::AccumulatedInput, prelude::*};

use crate::extras::{MovementExtrasPlugin, MovementExtrasState};
use crate::protocol::{AhoyButtons, AhoySnapshot, AhoyUserCmd, NetAhoyMoveState};

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
    pub extras_state: MovementExtrasState,
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
    extras_state: &'static mut MovementExtrasState,
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
    // Absent on sides that skip MovementExtrasPlugin (e.g. the client predictor),
    // so fall back to the shipped defaults rather than requiring the resource.
    extras_config: Option<Res<'w, MovementExtrasPlugin>>,
}

impl NetAhoyStepper<'_, '_> {
    /// Run one command through one movement step. `previous_buttons` lets us spot
    /// freshly pressed buttons (library bits for the controller, game bits for effects).
    pub fn step(
        &mut self,
        entity: Entity,
        command: AhoyUserCmd,
        previous_buttons: AhoyButtons,
    ) -> Result<()> {
        let fixed_delta = self.fixed_time.timestep();
        let extras_config = self.extras_config.as_deref().copied().unwrap_or_default();

        {
            let mut players = self.set.p1();
            let mut parts = players.get_mut(entity)?;
            tick_input_timers(&mut parts.input, fixed_delta);
            clear_transient_input(&mut parts.input);
            let previous_look = Vec2::new(parts.look.yaw, parts.look.pitch);
            apply_usercmd(&mut parts.input, &mut parts.look, command, previous_buttons);
            let airborne = parts.state.grounded.is_none();
            crate::extras::movement_pre_think(
                &command,
                previous_look,
                &extras_config,
                &mut parts.input,
                airborne,
            );
        }

        self.set.p0().step_entity(entity, fixed_delta)?;

        // The step writes Transform; Position is what the next step reads.
        let mut players = self.set.p1();
        let mut parts = players.get_mut(entity)?;
        parts.position.0 = parts.transform.translation;

        self.handle_extras(entity, &command, previous_buttons)
    }

    /// Run the movement extras (jump pads, rockets, ...) after the KCC step, so
    /// client replay and the server compose them identically. They need [`SpatialQuery`]
    /// (p2) alongside velocity/extras_state (p1), and those two can't be borrowed at
    /// once, so we copy velocity/extras_state out, run the extras, then write back.
    fn handle_extras(
        &mut self,
        entity: Entity,
        command: &AhoyUserCmd,
        previous_buttons: AhoyButtons,
    ) -> Result<()> {
        let config = self.extras_config.as_deref().copied().unwrap_or_default();
        let (position, look, mut velocity, mut extras_state) = {
            let mut players = self.set.p1();
            let parts = players.get_mut(entity)?;
            (
                parts.transform.translation,
                Vec2::new(parts.look.yaw, parts.look.pitch),
                parts.velocity.0,
                *parts.extras_state,
            )
        };

        crate::extras::movement_extras(
            position,
            look,
            command,
            previous_buttons,
            &self.set.p2(),
            &config,
            &mut extras_state,
            &mut velocity,
        );

        let mut players = self.set.p1();
        let mut parts = players.get_mut(entity)?;
        parts.velocity.0 = velocity;
        *parts.extras_state = extras_state;
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
            extras_state: *parts.extras_state,
        })
    }

    /// Rewind `entity` to an authoritative snapshot, reusing the locally
    /// recorded controller/input state for that tick when available.
    pub fn restore(
        &mut self,
        entity: Entity,
        snapshot: &AhoySnapshot,
        local_state: Option<(&CharacterControllerState, &AccumulatedInput, &MovementExtrasState)>,
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

        if let Some((stored_state, stored_input, stored_extras)) = local_state {
            *parts.state = stored_state.clone();
            *parts.input = stored_input.clone();
            *parts.extras_state = *stored_extras;
        } else {
            *parts.state = CharacterControllerState::default();
            *parts.input = AccumulatedInput::default();
            *parts.extras_state = MovementExtrasState::default();
        }

        snapshot.state.apply_to_controller_state(&mut parts.state);
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

    // Bits set this command but not last = rising edges.
    let pressed = command.buttons - previous_buttons;
    if pressed.contains(AhoyButtons::JUMP) {
        input.jumped = Some(Stopwatch::new());
    }
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

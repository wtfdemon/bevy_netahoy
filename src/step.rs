//! The movement context: one user command in, one movement step out.
//! Prediction, replay, and the server all go through [`NetAhoyStepper::step`].

use avian3d::prelude::*;
use bevy::{
    ecs::{query::QueryData, schedule::ScheduleLabel, system::SystemParam},
    prelude::*,
    time::Stopwatch,
};
use bevy_ahoy::{CharacterLook, input::AccumulatedInput, prelude::*};

use crate::protocol::{AhoyButtons, AhoySnapshot, AhoyUserCmd, NetAhoyMoveState};

/// A copy of a player (not a live query handle) handed to a [`Shove`],
/// so [`SpatialQuery`] can be borrowed alongside. Velocity is passed separately.
#[derive(Clone, Copy, Debug)]
pub struct MoveView {
    pub position: Vec3,
    /// `x` = yaw, `y` = pitch (radians).
    pub look: Vec2,
}

/// Per-player scratch handed to every [`Shove`] — a Quake-style POD blob for
/// shoves that need to remember something across ticks (e.g. a rocket in flight).
/// It rides the rollback frame, so prediction and replay stay exact. Keep it
/// minimal: add a field only when a shove actually needs it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct PendingShoves {
    /// Sequence a scheduled impulse is due to land. `armed == false` => idle.
    pub due_sequence: u32,
    /// Where the pending blast goes off (world space).
    pub explosion: Vec3,
    pub armed: bool,
}

/// A shove to the player — jump pad, rocket blast, anything. It sees only
/// replay-safe inputs and may change `velocity` and its [`PendingShoves`], both of
/// which ride the rollback frame, so it can't desync.
pub type Shove = fn(
    view: MoveView,
    command: &AhoyUserCmd,
    previous_buttons: AhoyButtons,
    world: &SpatialQuery,
    pending_shoves: &mut PendingShoves,
    velocity: &mut Vec3,
);

#[derive(Resource, Default)]
pub struct Shoves(pub Vec<Shove>);

/// Where Ahoy's own per-tick systems sit; netcode steps by hand via [`NetAhoyStepper`].
/// Add [`NetAhoyKccRunnerPlugin`] only if you also want Ahoy running every fixed tick.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct NetAhoyKccSchedule;

pub struct NetAhoyKccRunnerPlugin;

impl Plugin for NetAhoyKccRunnerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedPostUpdate,
            run_net_ahoy_kcc_schedule.before(PhysicsSystems::First),
        );
    }
}

fn run_net_ahoy_kcc_schedule(world: &mut World) {
    world.run_schedule(NetAhoyKccSchedule);
}

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
    pub pending_shoves: PendingShoves,
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
    pending_shoves: &'static mut PendingShoves,
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
    shoves: Res<'w, Shoves>,
    fixed_time: Res<'w, Time<Fixed>>,
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

        {
            let mut players = self.set.p1();
            let mut parts = players.get_mut(entity)?;
            tick_input_timers(&mut parts.input, fixed_delta);
            clear_transient_input(&mut parts.input);
            apply_usercmd(&mut parts.input, &mut parts.look, command, previous_buttons);
        }

        self.set.p0().step_entity(entity, fixed_delta)?;

        // The step writes Transform; Position is what the next step reads.
        let (view, mut velocity, mut pending_shoves) = {
            let mut players = self.set.p1();
            let mut parts = players.get_mut(entity)?;
            parts.position.0 = parts.transform.translation;
            (
                MoveView {
                    position: parts.transform.translation,
                    look: Vec2::new(parts.look.yaw, parts.look.pitch),
                },
                parts.velocity.0,
                *parts.pending_shoves,
            )
        };

        // Shoves (jump pads, rockets, ...) run here so client replay and server match.
        // `velocity`/`pending_shoves` are local copies, so p1 stays free while p2
        // (SpatialQuery) is borrowed.
        if !self.shoves.0.is_empty() {
            // Clone the (cheap, fn-pointer) list to drop the Res borrow before p2.
            let shoves = self.shoves.0.clone();
            let world = self.set.p2();
            for shove in &shoves {
                shove(
                    view,
                    &command,
                    previous_buttons,
                    &world,
                    &mut pending_shoves,
                    &mut velocity,
                );
            }

            let mut players = self.set.p1();
            let mut parts = players.get_mut(entity)?;
            parts.velocity.0 = velocity;
            *parts.pending_shoves = pending_shoves;
        }
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
            pending_shoves: *parts.pending_shoves,
        })
    }

    /// Rewind `entity` to an authoritative snapshot, reusing the locally
    /// recorded controller/input state for that tick when available.
    pub fn restore(
        &mut self,
        entity: Entity,
        snapshot: &AhoySnapshot,
        local_state: Option<(&CharacterControllerState, &AccumulatedInput, &PendingShoves)>,
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

        if let Some((stored_state, stored_input, stored_pending)) = local_state {
            *parts.state = stored_state.clone();
            *parts.input = stored_input.clone();
            *parts.pending_shoves = *stored_pending;
        } else {
            *parts.state = CharacterControllerState::default();
            *parts.input = AccumulatedInput::default();
            *parts.pending_shoves = PendingShoves::default();
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

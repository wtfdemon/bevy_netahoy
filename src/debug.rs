//! Debug/testing tooling: time scaling and a deterministic incoming-packet
//! conditioner. Not part of the netcode itself.

use std::time::Duration;

use aeronet::{
    io::{IoSystems, Session, SessionEndpoint, packet::RecvPacket},
    transport::TransportSystems,
};
use bevy::{platform::time::Instant, prelude::*};

pub const DEFAULT_DEBUG_SLOWMO_FACTOR: f32 = 0.1;
pub const MIN_DEBUG_TIME_SCALE: f32 = 0.01;
pub const MAX_DEBUG_TIME_SCALE: f32 = 4.0;

#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct DebugTimeScale {
    pub factor: f32,
}

impl Default for DebugTimeScale {
    fn default() -> Self {
        Self { factor: 1.0 }
    }
}

impl DebugTimeScale {
    pub fn new(factor: f32) -> Self {
        Self {
            factor: factor.clamp(MIN_DEBUG_TIME_SCALE, MAX_DEBUG_TIME_SCALE),
        }
    }

    pub fn is_scaled(self) -> bool {
        (self.factor - 1.0).abs() > f32::EPSILON
    }
}

pub fn debug_time_scale_from_args() -> DebugTimeScale {
    std::env::args()
        .find_map(|arg| {
            let rest = arg.strip_prefix("--slowmo")?;
            Some(DebugTimeScale::new(
                rest.strip_prefix('=')
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_DEBUG_SLOWMO_FACTOR),
            ))
        })
        .unwrap_or_default()
}

pub fn poor_network_from_args() -> bool {
    std::env::args().any(|arg| arg == "--poor-net")
}

pub fn apply_debug_time_scale(
    config: Res<DebugTimeScale>,
    mut virtual_time: ResMut<Time<Virtual>>,
) {
    virtual_time.set_relative_speed(config.factor);
    if config.is_scaled() {
        info!("debug time scale set to {:.3}x", config.factor);
    }
}

#[derive(Clone, Copy, Debug, Resource)]
pub struct NetworkConditionerConfig {
    pub incoming_latency: Duration,
    pub incoming_jitter: Duration,
    pub incoming_loss: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct AeronetNetworkConditionerPlugin {
    pub config: NetworkConditionerConfig,
}

impl AeronetNetworkConditionerPlugin {
    pub fn poor_condition() -> Self {
        Self {
            config: NetworkConditionerConfig {
                incoming_latency: Duration::from_millis(100),
                incoming_jitter: Duration::from_millis(15),
                incoming_loss: 0.10,
            },
        }
    }
}

impl Plugin for AeronetNetworkConditionerPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config)
            .add_observer(add_network_conditioner)
            .add_systems(
                PreUpdate,
                condition_session_packets
                    .after(IoSystems::Poll)
                    .before(TransportSystems::Poll),
            );
    }
}

#[derive(Component, Debug)]
pub struct NetworkConditioner {
    config: NetworkConditionerConfig,
    queued: Vec<ConditionedPacket>,
    ready: Vec<ConditionedPacket>,
    rng_state: u64,
}

impl NetworkConditioner {
    fn new(config: NetworkConditionerConfig, seed: u64) -> Self {
        Self {
            config,
            queued: Vec::with_capacity(64),
            ready: Vec::with_capacity(16),
            rng_state: seed.max(1),
        }
    }

    fn condition_packet(&mut self, packet: RecvPacket, now: Instant) {
        if self.next_unit_f32() < self.config.incoming_loss.clamp(0.0, 1.0) {
            return;
        }

        let delay = self.conditioned_delay();
        self.queued.push(ConditionedPacket {
            ready_at: now + delay,
            packet,
        });
    }

    fn collect_ready(&mut self, now: Instant) {
        self.ready.clear();

        let mut index = 0;
        while index < self.queued.len() {
            if self.queued[index].ready_at <= now {
                self.ready.push(self.queued.swap_remove(index));
            } else {
                index += 1;
            }
        }
    }

    fn conditioned_delay(&mut self) -> Duration {
        let latency_ms = self.config.incoming_latency.as_millis() as i128;
        let jitter_ms = self.config.incoming_jitter.as_millis() as i128;
        let jitter_ms = if jitter_ms == 0 {
            0
        } else {
            self.next_range_i128(-jitter_ms, jitter_ms)
        };

        Duration::from_millis((latency_ms + jitter_ms).max(0) as u64)
    }

    fn next_range_i128(&mut self, min: i128, max: i128) -> i128 {
        let width = (max - min + 1) as u128;
        min + (self.next_u64() as u128 % width) as i128
    }

    fn next_unit_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / ((1u32 << 24) as f32)
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.rng_state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.rng_state = value.max(1);
        self.rng_state
    }
}

#[derive(Debug)]
struct ConditionedPacket {
    ready_at: Instant,
    packet: RecvPacket,
}

fn add_network_conditioner(
    session: On<Add, SessionEndpoint>,
    mut commands: Commands,
    config: Res<NetworkConditionerConfig>,
) {
    let entity = session.event_target();
    let seed = entity.to_bits() ^ 0x9E37_79B9_7F4A_7C15;
    commands
        .entity(entity)
        .insert(NetworkConditioner::new(*config, seed));
}

fn condition_session_packets(mut sessions: Query<(&mut Session, &mut NetworkConditioner)>) {
    for (mut session, mut conditioner) in &mut sessions {
        let now = Instant::now();
        for packet in session.recv.drain(..) {
            conditioner.condition_packet(packet, now);
        }

        conditioner.collect_ready(now);
        conditioner.ready.sort_by_key(|packet| packet.ready_at);

        session
            .recv
            .extend(conditioner.ready.drain(..).map(|mut packet| {
                packet.packet.recv_at = now;
                packet.packet
            }));
    }
}

/// Render-rate C1-continuity probe for remote player visuals.
///
/// Samples each remote visual's final rendered translation once per frame
/// (`Last`, after avian's Hermite easing wrote it) and estimates per-frame
/// velocity and acceleration. Legit motion through the tick-rate Hermite is
/// curvature-bounded by |dv_tick| / tick_dt (a 7 m/s landing redirect at
/// 20 Hz reads as ~140 m/s^2); presentation faults (snaps, cursor pops,
/// extrapolation reversals) are step discontinuities that read as thousands.
/// Spikes above the threshold log immediately with clock context; a summary
/// line prints every 5 s so a quiet run still proves the probe was alive.
pub struct ContinuityProbePlugin;

/// Above the Hermite-curvature ceiling of legit motion: a hard landing or
/// rocket redirect renders as up to ~6*|dv|/tick_dt (~2-4k for a 20-30 m/s
/// redirect at 20 Hz). Only genuine presentation discontinuities exceed this.
const CONTINUITY_SPIKE_ACCEL: f32 = 5000.0;
/// Ignore sub-frame noise: a spike must also be a real velocity step.
const CONTINUITY_SPIKE_DV: f32 = 0.5;
const CONTINUITY_SUMMARY_SECONDS: f32 = 5.0;

#[derive(Resource, Default)]
struct ContinuityProbeState {
    tracks: bevy::platform::collections::HashMap<Entity, (Vec3, Vec3)>,
    window_seconds: f32,
    window_frames: u32,
    window_spikes: u32,
    window_max_accel: f32,
}

impl Plugin for ContinuityProbePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContinuityProbeState>()
            .add_systems(Last, probe_remote_continuity);
    }
}

fn probe_remote_continuity(
    time: Res<Time>,
    fixed_time: Res<Time<Fixed>>,
    mut clock: ResMut<crate::ClientServerClock>,
    remotes: Query<
        (Entity, &Transform, &crate::RemotePlaybackStatus),
        With<crate::RemotePlayerVisual>,
    >,
    mut state: ResMut<ContinuityProbeState>,
) {
    let dt = time.delta_secs();
    if dt <= f32::EPSILON {
        return;
    }

    let mut live = bevy::ecs::entity::EntityHashSet::default();
    for (entity, transform, status) in &remotes {
        live.insert(entity);
        let pos = transform.translation;
        let Some((prev_pos, prev_vel)) = state.tracks.get(&entity).copied() else {
            state.tracks.insert(entity, (pos, Vec3::ZERO));
            continue;
        };
        let vel = (pos - prev_pos) / dt;
        let dv = vel - prev_vel;
        let accel = dv.length() / dt;
        state.window_frames += 1;
        state.window_max_accel = state.window_max_accel.max(accel);
        if accel > CONTINUITY_SPIKE_ACCEL && dv.length() > CONTINUITY_SPIKE_DV {
            state.window_spikes += 1;
            info!(
                "continuity spike {entity}: |dv| {:.2} m/s accel {:.0} m/s^2 dt {:.1}ms alpha {:.3} pos {:.3?} extrap {} frozen {} clock lag {:.2}",
                dv.length(),
                accel,
                dt * 1000.0,
                fixed_time.overstep_fraction(),
                pos,
                status.extrapolating,
                status.frozen,
                clock
                    .target_time()
                    .map(|t| {
                        (clock.latest_server_tick as f64
                            - clock.interpolation_delay_seconds * clock.tick_hz)
                            - t.as_ticks_f64()
                    })
                    .unwrap_or(0.0),
            );
        }
        state.tracks.insert(entity, (pos, vel));
    }
    state.tracks.retain(|entity, _| live.contains(entity));

    state.window_seconds += dt;
    if state.window_seconds >= CONTINUITY_SUMMARY_SECONDS && state.window_frames > 0 {
        info!(
            "continuity summary: {} remote-frames, {} spikes, max accel {:.0} m/s^2 over {:.1}s | snaps {} extrap-frames {} head-jump-max {}",
            state.window_frames,
            state.window_spikes,
            state.window_max_accel,
            state.window_seconds,
            clock.stats.snaps,
            clock.stats.extrapolated_frames,
            clock.stats.head_jump_max,
        );
        clock.stats.head_jump_max = 0;
        state.window_seconds = 0.0;
        state.window_frames = 0;
        state.window_spikes = 0;
        state.window_max_accel = 0.0;
    }
}

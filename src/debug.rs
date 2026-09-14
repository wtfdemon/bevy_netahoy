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

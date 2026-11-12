//! Demos: a demo is the recorded incoming replicon byte stream, played back through the same
//! pipeline.
//!
//! Recording taps the messages the transport hands to replicon and writes them with their
//! arrival times. Playback has no network backend: [`NetAhoyDemoPlaybackPlugin`] is the
//! replicon messaging backend and feeds the recorded bytes in on the original schedule, so
//! replication, server events and interpolation run unmodified.
//!
//! Recording must start at connect: replicon's stream is delta-encoded against what the client
//! already acked, so a mid-session recording has no baseline. The same fact makes seeking
//! simple: a demo has one keyframe, its start. A backward [`DemoPlayback::seek`] ends the
//! session as a disconnect would, then replays every record up to the target in one frame.

use std::time::Duration;
use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use aeronet::transport::{Transport, TransportSystems};
use aeronet_replicon::client::{AeronetRepliconClient, ClientTransportSystems};
use bevy::prelude::*;
use bevy_replicon::{bytes::Bytes, prelude::*};

use crate::{client::RemoteClock, protocol::PlayerId, reconnect::reset_client_session};

const DEMO_MAGIC: &[u8; 8] = b"AHOYDEM1";

/// Records every server->client message to `path`. Add alongside the normal
/// network backend, before connecting.
pub struct NetAhoyDemoRecordPlugin {
    /// The file to write; created, or emptied if it exists.
    pub path: PathBuf,
}

impl Plugin for NetAhoyDemoRecordPlugin {
    fn build(&self, app: &mut App) {
        let path = self.path.clone();
        app.add_systems(
            Startup,
            move |mut commands: Commands, hash: Res<ProtocolHash>| match DemoRecorder::create(
                &path,
                &format!("{:?}", *hash),
            ) {
                Ok(recorder) => {
                    info!("recording demo to {}", path.display());
                    commands.insert_resource(recorder);
                }
                Err(err) => error!("failed to create demo file {}: {err}", path.display()),
            },
        )
        .add_systems(
            PreUpdate,
            record_incoming
                .after(TransportSystems::Poll)
                .before(ClientTransportSystems::Poll)
                .run_if(
                    resource_exists::<DemoRecorder>.and_then(resource_exists::<ClientMessages>),
                ),
        );
    }
}

/// The demo file being written; removed when a write fails, which stops the recording.
#[derive(Resource)]
pub(crate) struct DemoRecorder {
    writer: BufWriter<File>,
    start: Option<f64>,
}

impl DemoRecorder {
    fn create(path: &Path, protocol_hash: &str) -> io::Result<Self> {
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(DEMO_MAGIC)?;
        writer.write_all(&(protocol_hash.len() as u16).to_le_bytes())?;
        writer.write_all(protocol_hash.as_bytes())?;
        Ok(Self {
            writer,
            start: None,
        })
    }

    fn write_record(&mut self, now: f64, channel: usize, payload: &[u8]) -> io::Result<()> {
        let t = now - *self.start.get_or_insert(now);
        self.writer.write_all(&t.to_le_bytes())?;
        self.writer.write_all(&(channel as u16).to_le_bytes())?;
        self.writer
            .write_all(&(payload.len() as u32).to_le_bytes())?;
        self.writer.write_all(payload)
    }
}

/// Drains transport messages before aeronet_replicon's forwarder would,
/// records them, and forwards them itself. The real forwarder then finds an
/// empty buffer and no-ops (it still handles acks and connection state).
fn record_incoming(
    mut commands: Commands,
    mut recorder: ResMut<DemoRecorder>,
    mut messages: ResMut<ClientMessages>,
    mut sessions: Query<&mut Transport, With<AeronetRepliconClient>>,
    time: Res<Time<Real>>,
) {
    let now = time.elapsed_secs_f64();
    let mut result = Ok(());
    for mut transport in &mut sessions {
        for msg in transport.recv.msgs.drain() {
            let channel = aeronet_replicon::convert::to_channel_id(msg.lane);
            result = result.and_then(|()| recorder.write_record(now, channel, &msg.payload));
            messages.insert_received(channel, msg.payload);
        }
    }

    if let Err(err) = result.and_then(|()| recorder.writer.flush()) {
        error!("demo write failed, recording stopped: {err}");
        commands.remove_resource::<DemoRecorder>();
    }
}

/// Where a demo comes from: a file (native), or bytes already in memory (a
/// browser fetched them).
#[derive(Clone)]
pub enum DemoSource {
    /// A demo file on disk.
    File(PathBuf),
    /// A demo file's bytes.
    Bytes(std::sync::Arc<[u8]>),
}

impl DemoSource {
    fn describe(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Bytes(bytes) => format!("<{} bytes>", bytes.len()),
        }
    }

    fn read(&self) -> io::Result<(String, Vec<DemoRecord>)> {
        match self {
            Self::File(path) => read_demo(BufReader::new(File::open(path)?)),
            Self::Bytes(bytes) => read_demo(&bytes[..]),
        }
    }
}

/// Plays a demo back as the replicon messaging backend. Add it instead of the network backend
/// plugins: it owns [`ClientState`] and the message stream.
pub struct NetAhoyDemoPlaybackPlugin {
    /// The demo to play.
    pub source: DemoSource,
    /// Show only the last this-many seconds: everything before is applied at once on the first
    /// frame. `None` plays from the start.
    pub last_secs: Option<f64>,
}

impl NetAhoyDemoPlaybackPlugin {
    /// Plays the demo file at `path` from the start.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            source: DemoSource::File(path.into()),
            last_secs: None,
        }
    }
}

impl Plugin for NetAhoyDemoPlaybackPlugin {
    fn build(&self, app: &mut App) {
        let source = self.source.clone();
        let last_secs = self.last_secs;
        app.add_systems(
            Startup,
            move |mut commands: Commands,
                  hash: Res<ProtocolHash>,
                  mut next_state: ResMut<NextState<ClientState>>| {
                let name = source.describe();
                let (demo_hash, records) = match source.read() {
                    Ok(demo) => demo,
                    Err(err) => {
                        error!("failed to read demo {name}: {err}");
                        return;
                    }
                };
                let current_hash = format!("{:?}", *hash);
                if demo_hash != current_hash {
                    error!("demo {name} was recorded with an incompatible protocol");
                    return;
                }

                let mut playback = DemoPlayback::new(records);
                // Starting late is a seek from 0: the same one-frame burst,
                // announced the same way, so the game treats both alike.
                let start = last_secs.map_or(0.0, |last| (playback.duration() - last).max(0.0));
                playback.seek(start);
                info!(
                    "playing demo {name} ({} messages, {:.1}s, from {start:.1}s)",
                    playback.records.len(),
                    playback.duration(),
                );
                commands.insert_resource(playback);
                next_state.set(ClientState::Connected);
            },
        )
        .add_message::<DemoSeeked>()
        .add_systems(
            PreUpdate,
            (
                reset_for_seek.run_if(seeking_backward),
                apply_in_order,
                playback_inject,
            )
                .chain()
                .in_set(ClientSystems::ReceivePackets)
                .run_if(resource_exists::<DemoPlayback>.and_then(in_state(ClientState::Connected))),
        );
    }
}

/// A demo playing: its clock, speed and seeks. Present only during playback.
#[derive(Resource)]
pub struct DemoPlayback {
    /// Kept for the whole session, as a backward seek replays them; `Bytes`, so handing one to
    /// replicon again is a refcount, not a copy.
    records: Vec<DemoRecord>,
    cursor: usize,
    /// Demo time (s); advances by the virtual-time delta times `speed`, so a fixed-step render
    /// can drive it through `Time<Virtual>`.
    pub clock: f64,
    /// 1.0 plays at the recorded pace, 0.0 pauses.
    pub speed: f64,
    /// The player whose session this demo recorded, from the join the recording replays: nobody
    /// is local in playback, so [`OwnPlayerId`](crate::OwnPlayerId) stays `None` and a spectator
    /// camera aims with this.
    pub recorded_player: Option<PlayerId>,
    /// Where the next `PreUpdate` jumps to.
    seek_to: Option<f64>,
    /// The pending seek tore the session down (see [`DemoSeeked::reset`]).
    reset: bool,
    /// Remote render time minus the demo clock, in ticks: set when the remote clock first
    /// starts, kept across seeks.
    pub(crate) remote_clock_offset: Option<f64>,
    /// This frame's records are a catch-up burst (a seek, or a late start), not the stream
    /// arriving in real time.
    burst: bool,
}

impl DemoPlayback {
    fn new(records: Vec<DemoRecord>) -> Self {
        Self {
            records,
            cursor: 0,
            clock: 0.0,
            speed: 1.0,
            recorded_player: None,
            seek_to: None,
            reset: false,
            remote_clock_offset: None,
            burst: false,
        }
    }

    /// The demo's length (s).
    pub fn duration(&self) -> f64 {
        self.records.last().map(|record| record.t).unwrap_or(0.0)
    }

    /// Whether every record has been played.
    pub fn finished(&self) -> bool {
        self.cursor >= self.records.len()
    }

    /// Jumps to demo time `t` (clamped to the demo) in the next `PreUpdate`, before replication
    /// receives: [`Self::clock`] becomes `t` and the world is what straight playback has at `t`.
    /// Backward resets the client session and replays from the start; forward only injects the
    /// records in between. [`DemoSeeked`] follows that frame. The last request in a frame wins.
    pub fn seek(&mut self, t: f64) {
        self.seek_to = Some(t.clamp(0.0, self.duration()));
    }

    /// The seek waiting for the next `PreUpdate`, if any.
    pub fn pending_seek(&self) -> Option<f64> {
        self.seek_to
    }

    /// Whether the pending seek goes backward, so it tears the session down (every replicated
    /// entity despawned and spawned anew) before the replay.
    pub fn seek_resets(&self) -> bool {
        self.seek_to
            .is_some_and(|to| self.newest_record_time().is_some_and(|newest| newest > to))
    }

    /// True from the `PreUpdate` that applies a seek (its teardown included) until the next:
    /// what replicates and every server event this frame is history replayed at once, not
    /// news. Observers skip one-shot effects (a sound, a flash) on it, and still build state.
    pub fn bursting(&self) -> bool {
        self.burst
    }

    /// Arrival time of the newest record applied so far: the demo time the stream's newest
    /// server tick reached the recorder.
    pub fn newest_record_time(&self) -> Option<f64> {
        self.cursor
            .checked_sub(1)
            .map(|newest| self.records[newest].t)
    }
}

/// Written the frame a seek lands (readable in `Update`), after the burst went in: the cue to
/// reset client state replication doesn't own (effects, smoothing, caches).
#[derive(Message, Clone, Copy, Debug)]
pub struct DemoSeeked {
    /// The demo time the seek landed on (s).
    pub t: f64,
    /// The seek went backward: the session was rebuilt and every replicated entity is new.
    pub reset: bool,
}

fn seeking_backward(playback: Res<DemoPlayback>) -> bool {
    playback.seek_resets()
}

/// Where this frame's clock lands: a pending seek's time, else the clock moved on by the
/// virtual-time delta.
fn next_clock(playback: &DemoPlayback, time: &Time<Virtual>) -> f64 {
    playback
        .seek_to
        .unwrap_or(playback.clock + time.delta_secs_f64() * playback.speed)
}

/// Replays each of the recorder's receives (records sharing an arrival time) as one receive, in
/// order. One receive applies every update message before any mutation, then the mutations
/// newest first, skipping what an entity already has newer (replicon's `apply_replication`), so
/// a frame spanning several recorded receives would lose a component that changed once and was
/// followed by any newer message for its entity. Every group but the last goes through
/// replicon's receive here, in `OnEnter(Connected)` (the rest of that schedule only resets
/// clocks a seek resets anyway and sends what a demo drops); the last in this frame's normal
/// receive ([`playback_inject`]). A seek's burst, or a long frame, then leaves the world
/// exactly as the recorder saw it.
fn apply_in_order(world: &mut World) {
    let target = next_clock(
        world.resource::<DemoPlayback>(),
        world.resource::<Time<Virtual>>(),
    );
    let seeking = world.resource::<DemoPlayback>().pending_seek().is_some();
    loop {
        let playback = world.resource::<DemoPlayback>();
        let Some(first) = playback.records.get(playback.cursor) else {
            return;
        };
        let arrived = first.t;
        let end = playback.records[playback.cursor..]
            .iter()
            .position(|record| record.t != arrived)
            .map_or(playback.records.len(), |len| playback.cursor + len);
        // The last group due this frame is playback_inject's.
        if playback.records.get(end).is_none_or(|next| next.t > target) {
            return;
        }
        let group: Vec<_> = playback.records[playback.cursor..end]
            .iter()
            .map(|record| (record.channel as usize, record.payload.clone()))
            .collect();
        let mut playback = world.resource_mut::<DemoPlayback>();
        playback.cursor = end;
        // One-shot observers skip a seek's history (a backward one's teardown set it already).
        playback.burst |= seeking;
        let mut messages = world.resource_mut::<ClientMessages>();
        for (channel, payload) in group {
            messages.insert_received(channel, payload);
        }
        if let Err(err) = world.try_run_schedule(OnEnter(ClientState::Connected)) {
            error!("demo: no OnEnter(Connected) to receive with: {err}");
            return;
        }
        world
            .resource_mut::<ClientMessages>()
            .drain_sent()
            .for_each(drop);
    }
}

/// A backward seek ends the session as a disconnect would, without leaving
/// [`ClientState::Connected`]: `OnExit(Connected)` runs replicon's client reset and anything
/// else that cleans up after a session, and [`reset_client_session`] despawns every replicated
/// entity and clears the client state, the remote clock included. The burst then rebuilds it
/// all from record 0. Exclusive, so it's done before replicon's receive: nobody sees the empty
/// world.
fn reset_for_seek(world: &mut World) {
    // The teardown is part of the burst: observers of what it despawns
    // (`On<Remove, _>`) see a cut, not a game event.
    world.resource_mut::<DemoPlayback>().burst = true;
    if let Err(err) = world.try_run_schedule(OnExit(ClientState::Connected)) {
        error!("demo seek: no OnExit(Connected) to reset replicon with: {err}");
    }
    if let Err(err) = world.run_system_cached(reset_client_session) {
        error!("demo seek: session reset failed: {err}");
    }
    let mut playback = world.resource_mut::<DemoPlayback>();
    playback.cursor = 0;
    playback.reset = true;
}

fn playback_inject(
    mut playback: ResMut<DemoPlayback>,
    mut messages: ResMut<ClientMessages>,
    mut seeked: MessageWriter<DemoSeeked>,
    server_clock: Option<ResMut<RemoteClock>>,
    mut time: ResMut<Time<Virtual>>,
    mut generic: ResMut<Time>,
    mut fixed: ResMut<Time<Fixed>>,
) {
    let was_finished = playback.finished();
    // A seek lands exactly on its time: this frame's delta is spent on the jump.
    playback.clock = next_clock(&playback, &time);
    let seek = playback.seek_to.take();
    playback.burst = seek.is_some();

    while let Some(record) = playback.records.get(playback.cursor)
        && record.t <= playback.clock
    {
        messages.insert_received(record.channel as usize, record.payload.clone());
        playback.cursor += 1;
    }

    if let Some(t) = seek {
        let reset = std::mem::take(&mut playback.reset);
        // Even a forward jump re-seats the remote clock: slewing across the gap would play
        // remotes late for seconds. It starts again on this frame's newest tick.
        if let Some(mut server_clock) = server_clock {
            server_clock.reset();
        }
        // The fixed clock's phase follows the demo clock, whatever overstep the frames before
        // the seek left: two sessions stepping the same frames from different seek points run
        // their fixed steps on the same frames. Half a tick past the lattice point, so a frame
        // landing on one never ties against rounding.
        let timestep = fixed.timestep();
        let lattice = t.rem_euclid(timestep.as_secs_f64()) + timestep.as_secs_f64() * 0.5;
        let overstep = fixed.overstep();
        fixed.discard_overstep(overstep);
        fixed.accumulate_overstep(Duration::from_secs_f64(
            lattice.rem_euclid(timestep.as_secs_f64()),
        ));
        // `Time`'s elapsed is the demo clock from here, so everything keyed on it (shaders'
        // `globals.time`, animation phases) is the same in every session. The frame's delta
        // stays as it was.
        let mut fresh = Time::<Virtual>::from_max_delta(time.max_delta());
        fresh.set_relative_speed_f64(time.relative_speed_f64());
        if time.is_paused() {
            fresh.pause();
        }
        fresh.advance_by(Duration::from_secs_f64(t.max(0.0)));
        fresh.advance_by(time.delta());
        *time = fresh;
        *generic = time.as_generic();
        seeked.write(DemoSeeked { t, reset });
    }

    // Outgoing messages (replicon acks, the replayed JoinRequest) go nowhere.
    messages.drain_sent().for_each(drop);

    if playback.finished() && !was_finished {
        info!("demo finished ({:.1}s)", playback.duration());
    }
}

struct DemoRecord {
    t: f64,
    channel: u16,
    payload: Bytes,
}

fn read_demo(mut reader: impl Read) -> io::Result<(String, Vec<DemoRecord>)> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != DEMO_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an ahoy demo file",
        ));
    }
    let mut hash_len = [0u8; 2];
    reader.read_exact(&mut hash_len)?;
    let mut hash = vec![0u8; u16::from_le_bytes(hash_len) as usize];
    reader.read_exact(&mut hash)?;
    let hash = String::from_utf8(hash)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad protocol hash"))?;

    let mut records = Vec::new();
    loop {
        match read_record(&mut reader) {
            Ok(Some(record)) => records.push(record),
            Ok(None) => break,
            // A crash mid-write truncates the last record; play what's intact.
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                warn!("demo truncated mid-record; playing the intact part");
                break;
            }
            Err(err) => return Err(err),
        }
    }
    Ok((hash, records))
}

fn read_record(reader: &mut impl Read) -> io::Result<Option<DemoRecord>> {
    let mut t = [0u8; 8];
    if let Err(err) = reader.read_exact(&mut t) {
        return if err.kind() == io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(err)
        };
    }
    let mut channel = [0u8; 2];
    reader.read_exact(&mut channel)?;
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
    reader.read_exact(&mut payload)?;
    Ok(Some(DemoRecord {
        t: f64::from_le_bytes(t),
        channel: u16::from_le_bytes(channel),
        payload: payload.into(),
    }))
}

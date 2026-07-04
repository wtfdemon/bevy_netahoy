//! Source-style demos: a demo is the recorded incoming replicon byte stream,
//! played back through the same pipeline — the trick Quake and Source use.
//!
//! Recording taps the messages the transport hands to replicon and writes them
//! to disk with arrival timestamps. Playback runs with no network backend at
//! all: [`DemoPlaybackPlugin`] acts as the replicon messaging backend and feeds
//! the recorded bytes in on the original schedule, so replication, server
//! events, and interpolation all run unmodified and can't tell the server
//! isn't there.
//!
//! Recording must start at connect: replicon's stream is delta-encoded against
//! what the client already acked, so a mid-session recording has no baseline.
//! Reconnect to start a fresh demo.

use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use aeronet::transport::{Transport, TransportSystems};
use aeronet_replicon::client::{AeronetRepliconClient, ClientTransportSystems};
use bevy::prelude::*;
use bevy_replicon::prelude::*;

const DEMO_MAGIC: &[u8; 8] = b"AHOYDEM1";

/// Records every server->client message to `path`. Add alongside the normal
/// network backend, before connecting.
pub struct DemoRecordPlugin {
    pub path: PathBuf,
}

impl Plugin for DemoRecordPlugin {
    fn build(&self, app: &mut App) {
        let path = self.path.clone();
        app.add_systems(
            Startup,
            move |mut commands: Commands, hash: Res<ProtocolHash>| {
                match DemoRecorder::create(&path, &format!("{:?}", *hash)) {
                    Ok(recorder) => {
                        info!("recording demo to {}", path.display());
                        commands.insert_resource(recorder);
                    }
                    Err(err) => error!("failed to create demo file {}: {err}", path.display()),
                }
            },
        )
        .add_systems(
            PreUpdate,
            record_incoming
                .after(TransportSystems::Poll)
                .before(ClientTransportSystems::Poll)
                .run_if(resource_exists::<DemoRecorder>.and(resource_exists::<ClientMessages>)),
        );
    }
}

#[derive(Resource)]
pub struct DemoRecorder {
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
        self.writer.write_all(&(payload.len() as u32).to_le_bytes())?;
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

/// Plays a demo back as the replicon messaging backend. Add INSTEAD of the
/// network backend plugins (no websocket, no aeronet_replicon) — this plugin
/// owns [`ClientState`] and the message stream.
pub struct DemoPlaybackPlugin {
    pub path: PathBuf,
}

impl Plugin for DemoPlaybackPlugin {
    fn build(&self, app: &mut App) {
        let path = self.path.clone();
        app.add_systems(
            Startup,
            move |mut commands: Commands,
                  hash: Res<ProtocolHash>,
                  mut next_state: ResMut<NextState<ClientState>>| {
                let (demo_hash, records) = read_demo(&path)
                    .unwrap_or_else(|err| panic!("failed to read demo {}: {err}", path.display()));
                let current_hash = format!("{:?}", *hash);
                assert_eq!(
                    demo_hash,
                    current_hash,
                    "demo {} was recorded with an incompatible protocol",
                    path.display()
                );

                let playback = DemoPlayback::new(records);
                info!(
                    "playing demo {} ({} messages, {:.1}s)",
                    path.display(),
                    playback.records.len(),
                    playback.duration()
                );
                commands.insert_resource(playback);
                next_state.set(ClientState::Connected);
            },
        )
        .add_systems(
            PreUpdate,
            playback_inject
                .in_set(ClientSystems::ReceivePackets)
                .run_if(resource_exists::<DemoPlayback>.and(in_state(ClientState::Connected))),
        );
    }
}

#[derive(Resource)]
pub struct DemoPlayback {
    records: Vec<DemoRecord>,
    cursor: usize,
    /// Demo-time in seconds; advances by real delta * `speed`.
    pub clock: f64,
    /// 1.0 = original pacing, 0.0 = paused.
    pub speed: f64,
    /// Player id from the replayed [`JoinAccepted`](crate::protocol::JoinAccepted) —
    /// whose session this demo recorded. Nobody is "local" during playback;
    /// use this to aim a spectator camera.
    pub recorded_player: Option<u64>,
}

impl DemoPlayback {
    fn new(records: Vec<DemoRecord>) -> Self {
        Self {
            records,
            cursor: 0,
            clock: 0.0,
            speed: 1.0,
            recorded_player: None,
        }
    }

    pub fn duration(&self) -> f64 {
        self.records.last().map(|record| record.t).unwrap_or(0.0)
    }

    pub fn finished(&self) -> bool {
        self.cursor >= self.records.len()
    }
}

fn playback_inject(
    mut playback: ResMut<DemoPlayback>,
    mut messages: ResMut<ClientMessages>,
    time: Res<Time<Real>>,
) {
    let was_finished = playback.finished();
    playback.clock += time.delta_secs_f64() * playback.speed;

    while playback.cursor < playback.records.len()
        && playback.records[playback.cursor].t <= playback.clock
    {
        let cursor = playback.cursor;
        let record = &mut playback.records[cursor];
        let channel = record.channel as usize;
        let payload = std::mem::take(&mut record.payload);
        messages.insert_received(channel, payload);
        playback.cursor += 1;
    }

    // The backend's other half: outgoing messages (replicon acks, the replayed
    // JoinRequest) go nowhere.
    messages.drain_sent().for_each(drop);

    if playback.finished() && !was_finished {
        info!("demo finished ({:.1}s)", playback.duration());
    }
}

struct DemoRecord {
    t: f64,
    channel: u16,
    payload: Vec<u8>,
}

fn read_demo(path: &Path) -> io::Result<(String, Vec<DemoRecord>)> {
    let mut reader = BufReader::new(File::open(path)?);

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
        payload,
    }))
}

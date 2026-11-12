# bevy_netahoy

A Bevy netcode library for movement shooters built on
[bevy_ahoy](https://github.com/wtfdemon/bevy_ahoy/tree/netahoy)'s character controller and
[bevy_replicon](https://github.com/projectharmonia/bevy_replicon). It gives a game:

- client prediction, with rewind and replay on a server correction;
- server input rollback: a late command re-steps the ticks that ran on a guess for it;
- lag compensation: shots, the player push and moving platforms are judged where the player's
  screen showed them;
- an input clock that keeps each client's commands one to three ticks ahead of the server;
- a compact usercmd wire, delta-coded, with the unacked tail repeated on datagram links;
- reconnect in place, and demo recording and playback;
- events that survive replay: each is seen once, however often its command is stepped.

Three ideas carry it:

- A player is one plain struct with one step function. Prediction, replay and the server all run
  that function on that struct.
- Each player has a timeline clocked by their own commands: a command is one tick of that
  player's time, and the server steps each player's commands in order, one per tick.
- Each command carries what the player saw (the remote time their screen showed), so both sides
  rebuild the same world for it: other players, moving bodies and rays where that screen had
  them.

## How it works

1. Each fixed tick the client turns `ClientInput` into a `UserCmd` (movement, look, buttons, a
   sub-tick fire, and the remote time its screen showed), steps its own player on it and sends it.
2. The server steps every player's commands, one per tick, through the same function. A command
   that hasn't arrived by its tick is guessed from held input; when it lands, the guessed ticks are
   stepped again.
3. The server publishes every player's pose to every client (`PlayerSnapshot`), and its whole state
   and the newest command it stepped to its owner (`OwnerSnapshot`).
4. The client checks its prediction at that command. Past a few centimetres or on any game-state
   difference it rewinds to the server's state and replays the newer commands; the correction's
   delta is handed to the game to ease on screen (`PendingCorrection`). Remote players are drawn
   from their pose history, slightly in the past.

Both peers keep the same per-tick pose history of every player and moving body. A command's seen
time picks a point in it, so the step's rays (`Step::world`), the player push and the bodies it
collides with are where the player saw them.

A game's part is the game half of a player, a `Copy` struct stepped by one plain function, and
the events its steps make:

```rust
use bevy::{ecs::entity::MapEntities, prelude::*};
use bevy_netahoy::{EventId, PlayerGame, PlayerState, Step, StepEvent};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, MapEntities)]
struct Jumper { jumps: u32 }

/// A jump: its id and the player's count with it.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
struct Jumped(EventId, u32);

impl StepEvent for Jumped {
    fn id(&self) -> EventId { self.0 }
}

impl PlayerGame for Jumper {
    type Event = Jumped;
    type Context = (); // what else a step reads: a resource, a query

    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        step.pmove(&mut player.moves);    // bevy_ahoy's walk, among the movers as seen
        step.separate(&mut player.moves); // push out of other players
        if player.moves.since_jump == 0 {
            player.game.jumps += 1;
            let id = EventId { owner: step.owner(), sequence: step.command().sequence };
            step.emit(Jumped(id, player.game.jumps));
        }
    }
}
```

`examples/minimal.rs` is this game whole, server and client.

Prediction frames, rollback and the owner snapshot copy `PlayerState<G>` (bevy_ahoy's `MoveState`
plus `G`) whole. Anything else a step reads comes through the `Step`: its game's `Context` (a
read-only system parameter) for data that is fixed or changes the same way on both peers at a known
tick, and a `History` at the command's seen time for anything that changes during play. Read
anything else and it doesn't rewind, and the peers disagree.
A step's events are plain Bevy messages (`MessageReader<Jumped>`). An event's key is its id and its
outermost enum variant, one event per key: give each kind of event (a fire, a hit) its own top-level
variant, don't nest kinds; a struct event type like `Jumped` gets one event per id. State follows
the last run, events follow the first: a replay or a rollback steps the player again and keeps the
new state, but an event among the last 1024 that went out on that player's timeline is never sent
again, even when the rerun moves it to a later command (a guessed direct hit that the real command
misses, so the rocket blasts at its fuse instead). The repeats are dropped where the timeline lives,
the server's command buffer or the client's session. The server needs that only because of input
rollback: with it off, the server never reruns a step. The server sends each event to every client
as `RemoteEvent<E>`: the verdict. For a client's own player (the event id's owner is its
`OwnPlayerId`) it can be the only copy, of an event the prediction never made: a guessed hit the
server kept, a hit on a pose the client only extrapolated. Play the predicted copy at once, and use
the server's to confirm it or to fill in what the prediction missed.

## Design

The model is Quake 3's and Source's: a usercmd / playerState / pmove split (one command in, one
step of one plain struct out), client-side prediction, lag compensation against a pose history,
and demos as recorded server streams. The input clock and the server's rollback of late input
follow Overwatch's
([GDC 2017](https://www.gdcvault.com/play/1024001/-Overwatch-Gameplay-Architecture-and)).

- One function steps a player everywhere: prediction, replay and the server.
- A rollback re-steps players, not the world: the stepper is one `SystemParam`, with no exclusive
  systems and no schedule re-runs.
- A published pose is final. Input rollback corrects the server and the owner, never what the
  others were sent, so both peers keep one history.

## Getting started

### Dependencies

Bevy 0.19, Avian 0.7, bevy_replicon 0.44 and aeronet 0.21. bevy_netahoy depends on its bevy_ahoy
fork by path, `../bevy_ahoy`: the
[`netahoy` branch of wtfdemon/bevy_ahoy](https://github.com/wtfdemon/bevy_ahoy/tree/netahoy),
which adds a manual stepper, the moves as plain functions, and movers. Clone both side by side
and depend on both by path, so the game and netahoy share one bevy_ahoy (or leave bevy_ahoy out
of the game's dependencies and use netahoy's re-export, `bevy_netahoy::bevy_ahoy`):

```bash
git clone https://github.com/wtfdemon/bevy_netahoy
git clone -b netahoy https://github.com/wtfdemon/bevy_ahoy
```

```toml
[dependencies]
aeronet = "0.21"
aeronet_replicon = { version = "0.21", features = ["client", "server"] }
aeronet_websocket = { version = "0.21", features = ["client", "server"] }
avian3d = "0.7"
bevy = "0.19"
bevy_ahoy = { path = "../bevy_ahoy" }
bevy_netahoy = { path = "../bevy_netahoy" }
bevy_replicon = "0.44.3"
serde = { version = "1", features = ["derive"] }

[patch.crates-io]
aeronet = { git = "https://github.com/wtfdemon/aeronet", rev = "aeb846c749717ea7ea3fdfc0b7f3f427c39ed8eb" }
aeronet_io = { git = "https://github.com/wtfdemon/aeronet", rev = "aeb846c749717ea7ea3fdfc0b7f3f427c39ed8eb" }
aeronet_transport = { git = "https://github.com/wtfdemon/aeronet", rev = "aeb846c749717ea7ea3fdfc0b7f3f427c39ed8eb" }
aeronet_replicon = { git = "https://github.com/wtfdemon/aeronet", rev = "aeb846c749717ea7ea3fdfc0b7f3f427c39ed8eb" }
aeronet_webtransport = { git = "https://github.com/wtfdemon/aeronet", rev = "aeb846c749717ea7ea3fdfc0b7f3f427c39ed8eb" }
aeronet_websocket = { git = "https://github.com/wtfdemon/aeronet", rev = "aeb846c749717ea7ea3fdfc0b7f3f427c39ed8eb" }
```

Copy the `[patch.crates-io]` block from this repo's `Cargo.toml`: patches only apply in the root
workspace. netahoy runs on an aeronet fork for bevy_replicon 0.44 with send backpressure; without
the block, crates.io's `aeronet_replicon` pulls in a second, older bevy_replicon. Patch all six: a
partial patch leaves two `aeronet_io`, whose sessions the other crates don't see.

These are `examples/minimal.rs`'s dependencies: `aeronet_replicon` and `aeronet_websocket` with
both sides (or another aeronet transport), and bevy's default features for a window.

### Plugins, in order

```rust
app.add_plugins((
    DefaultPlugins, // a headless server: MinimalPlugins, StatesPlugin, TransformPlugin, LogPlugin
    PhysicsPlugins::default(),
    AhoyPlugins::new(KccSchedule), // bevy_ahoy's systems on a schedule nothing runs
    RepliconPlugins,               // before the protocol
    NetAhoyProtocolPlugin::<Jumper>::new(TickRate::new(20)),
));

// The server.
app.add_plugins((
    WebSocketServerPlugin,
    AeronetRepliconServerPlugin,
    NetAhoyServerPlugin::<Jumper>::default(),
))
.set_runner(one_tick_per_frame);

// A client. Its sessions get `AeronetRepliconClient`, its input goes in
// `NetAhoyClientSystems::GatherInput`, and `Reconnect` runs its connect system again whenever
// the link drops.
app.add_plugins((
    WebSocketClientPlugin,
    AeronetRepliconClientPlugin,
    NetAhoyClientPlugin::<Jumper>::default(),
));
let connect = app.register_system(connect);
app.insert_resource(Reconnect::new(connect));
```

netahoy steps the controllers itself, so bevy_ahoy goes on `KccSchedule`, which nothing runs;
the client and server plugins panic at startup if bevy_ahoy's controller runs on any other
schedule. (The fork's `AhoyPlugins::manual()` is for a game that steps controllers itself without
netahoy.)

### WebTransport, with WebSocket where it fails

The `transport-client` and `transport-server` features give a game its sessions: WebTransport
first, and WebSocket where UDP doesn't get through, natively and in the browser. Both are off by
default, and `transport-server` is native only, so a headless server pulls in no browser crates
and a wasm client no QUIC stack.

```toml
bevy_netahoy = { path = "../bevy_netahoy", features = ["transport-server"] } # the server
bevy_netahoy = { path = "../bevy_netahoy", features = ["transport-client"] } # a client
```

Each plugin goes in place of aeronet's plugins for that side:

```rust
use bevy_netahoy::transport::*;

// The server: both listeners and the offer endpoint, at startup.
app.add_plugins(NetAhoyTransportServerPlugin::new(ServerTransportConfig::from_env()));

// A client: it connects at startup and reconnects through `Reconnect`.
app.add_plugins(NetAhoyTransportClientPlugin::from_settings());
```

The server listens on loopback by default: WebSocket on TCP 5000, WebTransport on UDP 5001, and
on TCP 5002 a small HTTP endpoint that answers its self-signed certificate's hash and its UDP
port, so a browser trusts the certificate with nothing to install. It renews the certificate
within the browsers' 14-day limit, and open sessions keep theirs. A client whose handshake fails,
or whose UDP alone dies mid-game, reconnects over WebSocket. Every session carries its
`Transport`, on both peers.

| setting | side | |
|---|---|---|
| `SERVER_BIND_ADDR=<ip>:<port>` | server | the WebSocket (default `127.0.0.1:5000`); WebTransport listens on its IP |
| `WT_PORT=<port>`, `WT_BIND_ADDR=<ip>:<port>` | server | WebTransport's port (default 5001), or its whole address |
| `WT_HASH_ADDR=<ip>:<port>` | server | the offer (default `127.0.0.1:5002`) |
| `SERVER_URL=<ws url>`, `WT_HASH_URL=<http url>` | native client | the WebSocket (default `ws://127.0.0.1:5000`) and the offer (default port 5002 on its host) |
| `TRANSPORT=auto\|wt\|ws`, `?transport=` | client | WebTransport first (`auto`, the default), or only one of them |

A browser client finds its server from the page's address: over https behind the page's proxy,
at `ws` and `wt` beside the page; over plain http on the page's host, on the default ports. The
`transport` module's docs have the rest: when a client falls back, the offer, and connecting
later than startup. The plugins run on netahoy's aeronet fork, so a game copies the
`[patch.crates-io]` block from netahoy's `Cargo.toml`: Cargo patches don't reach dependents.

### Spawning players

On the server, for each client that joins (`On<FromClient<JoinRequest>>`), spawn:

- `PlayerId`, unique, and `PlayerOwner(client)`. The owner brings `Replicated`, `ServerPlayer` and
  the rest of a networked player, and tells the client which player is its own;
- `PlayerState::<G>::at(position)`;
- a `CharacterController` whose filter is `WORLD_COLLISION_LAYER` only, `PlayerShape::collider()`,
  and `CollisionLayers::new(PLAYER_COLLISION_LAYER, ..)`.

The level goes on `WORLD_COLLISION_LAYER`, the same on both peers. On a client, when `OwnPlayer`
is added, spawn `PredictedPlayer { server_entity }` with the same controller and collider;
netahoy gives it the rest. Each `RemotePlayer` can get a `RemoteVisual`, which netahoy moves. A
spawn without an id, a client's player or prediction without a controller, or a player collider on
the world layer is an error. Spawn one player per client: a repeated `JoinRequest` must not add
another.

A player lives as long as its client's connection: the server despawns it when the client's
`ConnectedClient` goes, by a disconnect, a timeout or a refusal, and at once if it was spawned for
anything but an `AuthorizedClient` (replicon refuses a client whose protocol differs: another build
or another tick rate). The game has nothing to clean up. A bot owned by no client,
`PlayerOwner(Entity::PLACEHOLDER)`, stays until the game despawns it.

### Moving bodies

A step meets a moving body (a platform, a door) where its pose history had it at the command's
seen time, so both peers collide with it where the player saw it. Each one needs that history:

- on the server, spawn it `Replicated`, with `BodySnapshot::default()`, a `RigidBody::Kinematic`
  and its collider on `WORLD_COLLISION_LAYER`, and move its `Position` and `Rotation`: netahoy
  publishes them every tick into its `History<BodySnapshot>`, which replicates;
- on a client, give the replicated entity the same collider on `WORLD_COLLISION_LAYER`, and draw
  it at `body_at(history, time, rate)` for the frame's `PresentedFrame::remote`.

A body that moves on `WORLD_COLLISION_LAYER` without a `BodySnapshot` is met where each peer has
it live, and the peers disagree; one only a client has (an effect) stays off that layer. The
example's platform is set up this way: `spawn_platform` in `examples/playground/server.rs`,
`attach_platform` and `place_platform` in its `client/presentation.rs`.

### Game state in a history

Game state that changes during play and is read at a time, by a step or by drawing (a door, a
gate, a capture point), goes in a component the server writes in the fixed schedule, kept in a
history on both peers like a body's pose:

```rust
#[derive(Component, Clone, Copy, Serialize, Deserialize)]
struct Gate { raised: bool }

impl Recorded for Gate {}

app.replicate_with_history::<Gate>(); // on both peers, after NetAhoyProtocolPlugin
```

The server records every entity's `Gate` each tick into its `History<Gate>`, as it records the
bodies, and a client records what replication brings under the tick that sent it, so the entry at
a tick is the same on both peers. `history.sample(time)` gives the entries either side of a time:
`from` is the value in force, and the game blends `from` toward `to` by `alpha` if it
interpolates. Draw at the frame's `PresentedFrame::remote`; a step reads it through its game's
context at its command's seen time, the time the player drew it at:

```rust
impl PlayerGame for Arsenal {
    type Context = Query<'static, 'static, &'static History<Gate>>;
    fn step(player: &mut PlayerState<Self>, step: &mut Step<Self>) {
        let raised = step.context().iter()
            .any(|gate| gate.sample(step.seen_time()).is_some_and(|seen| seen.from.raised));
        // ...
    }
}
```

The example's shield gate is set up this way (`ShieldGate` in `examples/playground/game.rs`).

## Examples

See [examples/README.md](examples/README.md).

```bash
cargo run --example minimal -- --server
cargo run --example minimal            # as many clients as you like

cargo run --example playground_server  # the playground: rockets, a pistol, two bots
cargo run --example playground_client
just play                              # its server and 2 clients on a conditioned link
just web                               # its browser client on http://localhost:8080
```

On Linux, the continuity harness runs a server and headless autopilot clients under kernel netem
in a rootless namespace, then summarizes their logs. It needs `unshare`, iproute2, Xvfb and a
software Vulkan driver for the clients' renderer: Mesa's lavapipe (`mesa-vulkan-drivers` on
Debian and Ubuntu, `/usr/share/vulkan/icd.d/lvp_icd.json`; `VK_ICD_FILENAMES` points at another).

```bash
BUILD=1 scripts/continuity-linux.sh 2 60 harsh   # players, seconds, link profile
scripts/continuity-analyze.py <the log dir it printed>
```

The link profiles are `clean`, `lan`, `transatlantic`, `bursty`, `harsh`, `clumsy15`, `clumsy5`,
`brutal` and `stall` (the script's header says what each does). The clients run over WebSocket
unless `TRANSPORT=wt`.

The playground runs on the transport features; its [README](examples/README.md#transport) says
how it connects and how to pin one transport.

The tests: `cargo nextest run --workspace`.

## Tick rate

20 Hz unless the game passes another `TickRate` (10 to 1000 Hz) to `NetAhoyProtocolPlugin::new`.
Set it there, never on `Time<Fixed>`: the plugin panics if the two disagree. The rate is part of
the protocol hash, so the server and its clients must use the same one. Each tick is one command
and one published pose per player. netahoy sizes its windows (history, replay, the input clock) in
seconds. On the server, `one_tick_per_frame` runs one fixed tick per frame, so every tick's
snapshot is sent.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.

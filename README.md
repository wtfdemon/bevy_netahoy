# bevy_netahoy

The meant-to-be-forked prediction & rollback library for Bevy movement shooters,
stealing the best of Quake 3 and Source. Strafe jumping, bhopping, surfing,
sub-tick lag-compensated hitscan and rockets, rocket jumps. Predicted,
replayed, reconciled, and smooth in a web browser.

Built on [bevy_ahoy](https://github.com/janhohenheim/bevy_ahoy) and Avian 3D's
[`move_and_slide`](https://github.com/avianphysics/avian/pull/894).

Live demo with two browser windows. Servers hosted in Ashburn, USA, both clients in Berlin, Germany.
Transatlantic rtt, loss, and jitter. Players look smooth, inputs feel instant.



https://github.com/user-attachments/assets/4a30f002-0c6e-49b0-b40c-2c95c4b4cf2d



Live at https://demons.wtf

**WebSocket only, by measurement.** Chrome's WebTransport runs mandatory QUIC
congestion control over its datagrams ([RFC 9221 §5](https://www.rfc-editor.org/rfc/rfc9221.html)):
under real-world loss it *withholds* your "unreliable" packets in a send queue
instead of dropping them, starving the server of fresh input and forcing
rollbacks. TCP's retransmit delay, by contrast, is exactly what the
prediction/de-jitter stack absorbs. A/B tested under packet-level abuse in the
same browser, and reproduced deterministically in `tests/link_sim.rs` —
WebSocket won decisively. Not advised to re-enable WebTransport without
re-reading that test.

## Features

Prediction and reconciliation:

- Client prediction with rewind + replay, 256-frame history. Inputs ride
  one command per packet (40 B): on TCP the wire redelivers everything, so
  Q3-style command redundancy buys nothing and only floods thin uplinks
  (measured in `tests/link_sim.rs`).
- Server-side input handling, Overwatch[^1]/Rocket League[^2] style: one
  command per tick, no de-jitter buffer. When a command hasn't arrived by
  its tick, the server extrapolates held intent (one-shot payloads
  stripped, gravity keeps integrating — no mid-air freezes). Late commands
  replace those guessed ticks through bounded per-player rollback, and
  corrected snapshots revise remote interpolation history before it is
  rendered.
- Corrections you don't see: sub-3.5cm errors are accepted as-is, the rest
  fold into a critically damped error bridge on a separate presentation
  entity, and only teleport-class misses hard-snap (a 2.25m floor that
  widens with speed, so a 3m correction mid-bhop at 30m/s still hides).
  This is most of "smooth in a browser".
- Rollback triggers on gameplay state too, not just position: a
  server-declined fire or disputed hit forces a replay at zero position error.

Sub-tick and lag compensation:

- Fire clicks latch the fixed-clock overstep fraction and exact look angles,
  so a shot lands where the barrel was mid-tick, not where the tick started.
- "You hit what you saw": commands carry the interpolated time your screen
  showed, both peers sample the same pose history. Even at 144hz on a 20hz
  tickrate, the server reproduces your exact screen at the click.
- Predicted rockets are closed-form and sweep the whole gap since the last
  command, so they can't tunnel across lost packets. Self-knockback
  predicts: rocket jumps feel instant.
- Velocity-space player separation: rigid-body player collisions desync, so
  each player clips their own approach velocity against intersecting players
  and drains overlap as a capped velocity instead. Deterministic via the
  lag-comp history, so it predicts rollback-free. And you can shove people,
  which is fun.

Remote players:

- Source-style three-point cubic Hermite interpolation with tangents derived
  from observed positions (never the published velocity): a 20hz jump arc
  renders as an arc, not chords, and velocity stays continuous through
  turns. Falls back to lerp across gaps.
- Three-tick interpolation delay (150 ms: enough to eat one WebSocket
  retransmit stall) followed by capped extrapolation (0.30s, then hold). The
  playback clock advances exactly one server tick per fixed tick and slews a
  few percent to re-center, so remotes never crawl or stutter after the 20 Hz
  arrival staircase; a full outage holds, then resets in one jump.
- Server-side input rollback revises remote history: a late command replaces
  the guessed ticks and republishes them, and the viewer folds the revision
  into a critically damped error bridge so the rendered path stays C1
  (`tests/snapshot_sim.rs` measures hitches, stalls, and cap contact under
  simulated loss).

The rest:

- Interest management: the heavy reconcile snapshot goes only to its owner.
  Change-only body snapshots, so parked props go silent on the wire.
- Source-style demos: recording taps the raw replicon byte stream, playback
  is a fake network backend the unmodified client runs against. Speed,
  pause, spectator cam.
- A deterministic seeded network conditioner (`--poor-net` is the video
  above), clock/extrapolation stats in the logs, slow-mo, F3 server-truth
  ghosts, a wire-size regression test.
- The example game: lag-compensated hitscan with predicted-vs-acked hit
  markers, an owner-authoritative buggy, predicted gravity-gun
  grab/hold/throw, wasm transport, and a movement showcase map.

## Architecture

The core is a Quake 3 `bg_pmove`: one user command in, one movement step out.

```
AhoyUserCmd ──▶ NetAhoyStepper::player_move ──▶ KCC step ──▶ step_player_state
                (step.rs, the pmove)                         (player.rs, your game)
```

The same function runs everywhere: client prediction, client replay after a
server correction, and the server draining its command queue. That is the
whole determinism story. One code path, nothing to desync.

Like Q3's `PM_AddEvent`, the pmove emits `NetAhoyPlayerEvents` (fires, hits).
The server broadcasts them, the client drains them for predicted visuals.

Two deliberate choices:

- **No exclusive systems.** Rollback replays the pmove, not the world.
  Settled decision, see below.
- **POD, not generics.** Player state is a concrete struct
  (`NetAhoyPlayerState`), cloned whole into the prediction frame and sent as
  a subset on the wire. There is no `trait Rollbackable`. When your game
  needs different state, you fork this repo and edit the struct. That is the
  supported workflow, not a compromise.

### Why no exclusive systems

I tried exclusive (`&mut World`) rollback, profiled it, and even read the
goddamn assembly before dropping it. Please don't PR it back in.

An exclusive system is a hard sync point. The executor drains every in-flight
system before it and restarts the pipeline after, so the hitch is sized by
whatever happened to be running. That makes it spiky and unpredictable, and
you can't catch it in a profiler because the cost lives between functions.

Replaying corrections by re-running schedules is the expensive part. One
schedule run costs around 800 instructions of pure dispatch (label interning,
hashmap lookups, memcpying the `Schedule` in and out of the world) before any
system executes, plus an indirect `Box<dyn System>` call per system per
replayed tick. `dyn` is opaque to the optimizer, so none of it ever inlines.
The usual companion, building a `SystemState` per call, is Bevy's borrow
checking executed at runtime: roughly 790 instructions and heap allocations,
every time.

The `SystemParam` stepper does all of that once, at schedule build. The
scary-looking borrow choreography in `step.rs` compiles down to a bit test
and pointer math, about 45 instructions per entity access. A correction costs
pmove x ticks instead of world x ticks. On wasm that ratio is everything:
with one thread the sync point cost vanishes, and replay scope alone is what
separates smooth in a browser from stutter.

### The POD rule

POD = Plain Old Data. Meaning everything is thrown into one big classic C 
style struct. Everything predicted, at least. A great example is the predicted 
rockets, something that classically would make sense as an entity becomes a 
vector of rockets inside our `playerstate_t` style struct. Why? That way, they 
ride both rollback and prediction, and client/server truth stay in sync. 

Quake 3 kept everything in one big
`playerstate_t`. The Source engine tried to do it differently, decoupling
things like weapon and ammo state from player state, and that was actually a
bit of a mistake: it made maintaining the prediction and replay apparatus
harder. Here, we got the better deal.

The client and server may replay whole bursts of commands after a correction, so inside
`player_think`, "now" means this command, not this frame. The server is
still exactly one new pmove per player per fixed tick; a late command can also
rewind and replace bounded speculative ticks before that new step.

So any state your predicted logic touches must ride along: captured into the
prediction frame, restored on rewind, replayed per command, compared against
the server's snapshot. `NetAhoyPlayerState` is the one struct that does all
of that. The contract is simple: **if your gameplay state lives in the POD
and `player_think` steps it deterministically, it works everywhere
(prediction, replay, server) for free.**

Querying some Bevy component or resource from inside the pmove is the
opposite. It doesn't rewind, it may exist on one peer and not the other, and
it silently desyncs. Every such query is a misprediction waiting for packet
loss to expose it. This is also the deeper reason exclusive systems are the
wrong tool here: wanting `&mut World` inside a pmove usually means reaching
for state that can't ride rollback. The world data the pmove does read
(level geometry through `SpatialQuery`, the lag-compensation pose history) is
the library's job to keep mirrored on both peers. Your job is the POD.

That said, not everything has to ride the POD. It's a choice, per thing. The
question is just: do you want it predicted, instant, not waiting on a round
trip? Rockets here answer yes, so they live in the POD and the firer feels
the shot and the self-knockback the same frame the button goes down. A
grenade or an exploding barrel that shoves players around can answer no and
just be a plain Bevy entity with a plain system. It reacts a round trip
later, and for most things nobody notices. (You could predict those too if
you want to go crazy, the POD is right there.) We even leave some rocket
stuff unpredicted: a remote player's rocket knocking you around just takes
the snap correction, Quake-style, and it feels fine.

Map: `step.rs` pmove · `player.rs` game POD + events · `protocol.rs` wire
types · `client.rs` predict/replay/interp · `server.rs` authority + visibility
· `math.rs` lag-comp pose history · `demo.rs` demos as recorded replicon
streams.

## Extending it

- **New predicted gameplay** goes in `step_player_state` (`player.rs`):
  weapons, rockets in this case, anything that must survive rollback. Add its
  state to `NetAhoyPlayerState` and the snapshot, and prediction and
  reconciliation come for free.
- **Custom movement** goes in your `bevy_ahoy` fork. The stepper drives
  whatever the KCC does, so new mechanics (mantling, tac, whatever) just work
  through prediction.
- **Not everything needs the pmove.** The buggy in `examples/ahoy_replicon`
  is Source/Roblox-style owner-authoritative: boarding despawns your KCC and
  the driver's client owns the vehicle pose outright. Zero input latency,
  real physics, no rollback. The right tool when server authority isn't
  worth it.

## Pitfalls

- **Every replicated type lives in one shared lib.** Replicon's protocol hash
  uses `type_name`, so the same struct declared per-binary hashes differently
  and the client gets rejected as "non-authorized".
- **Anything the pmove reads must exist identically on both peers.** Don't
  `Option`-wrap a missing component. Insert it on client and server both, or
  every divergent default becomes a misprediction.
- **A missing visibility-filter component means visible to everyone**, not
  invisible (the docs read the other way). Server-spawned entities like bots
  need a placeholder owner.
- **Replicated physics bodies: `Dynamic` on the server, `Static` on the
  client.** Kinematic adds nothing for KCC stepping. Don't replicate Avian's
  `Position`/`Velocity` or Bevy `Transform`s, and don't smooth remote bodies
  with Avian's `TransformInterpolation`. Put a `BodySnapshot` on the wire
  and Hermite-interpolate that.
- **The KCC is a kinematic `RigidBody`.** Never use "has a RigidBody" to mean
  "is not a player". Splash damage applied through that filter hits players
  with dynamic-body math (ask us about the 40x rocket knockback).

## Run

```bash
cargo run --example ahoy_server
cargo run --example ahoy_client
```

See [examples/README.md](examples/README.md) for the WSL2 native-Windows and
browser/WebSocket paths.

[^1]: Timothy Ford, [*Overwatch Gameplay Architecture and Netcode*](https://www.gdcvault.com/play/1024001/-Overwatch-Gameplay-Architecture-and), GDC 2017.
[^2]: Jared Cone, [*It IS Rocket Science! The Physics of Rocket League Detailed*](https://www.gdcvault.com/play/1024972/It-IS-Rocket-Science-The), GDC 2018.

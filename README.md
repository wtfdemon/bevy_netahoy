# bevy_netahoy

The meant-to-fork prediction & rollback library for Bevy movement shooters,
stealing the best of Quake 3 and Source. Strafe jumping, bhopping, surfing,
sub-tick lag-compensated hitscan and rockets, rocket jumps. Predicted,
replayed, reconciled, and smooth in a web browser.

Built on [bevy_ahoy](https://github.com/janhohenheim/bevy_ahoy) and Avian 3D's
[`move_and_slide`](https://github.com/avianphysics/avian/pull/894).

Prediction under 200 ping, 10% packet loss, at a 20hz tickrate:

https://github.com/user-attachments/assets/348c77c7-0479-4286-b2ff-f13a10579a65

## Features

Prediction and reconciliation:

- Client prediction with rewind + replay, 256-frame history, inputs sent
  with 8-command redundancy so packet loss doesn't drop keystrokes.
- Corrections you don't see: sub-3.5cm errors are accepted as-is, the rest
  smooth in through a separate presentation entity, and only misses past
  2.25m hard-snap. This is most of "smooth in a browser".
- Rollback triggers on gameplay state too, not just position: a
  server-declined fire or disputed hit forces a replay at zero position error.

Sub-tick and lag compensation:

- Fire clicks latch the fixed-clock overstep fraction and exact look angles;
  the muzzle pose is re-derived identically on both peers, so a shot lands
  where the barrel was mid-tick, not where the tick started.
- "You hit what you saw": commands carry the interpolated time your screen
  showed, both peers sample the same pose history, and the server clamps
  claims to the rewind window so honest values pass through bit-identical.
  Even on a 144hz monitor at a 20hz tickrate, the server can reproduce
  where the other players were on your screen when you clicked.
- Predicted rockets are closed-form (position is a pure function of elapsed
  ticks), sweep the whole gap since the last command so they can't tunnel
  through players across lost packets, and self-knockback predicts, so
  rocket jumps feel instant.
- TF2-style player separation. Rigid-body player collisions desync (where
  the client thinks players are vs where the server does), which is why most
  arena shooters just let players clip through each other. Instead, each
  player pushes themselves away from players they intersect. Deterministic
  thanks to the lag-comp history, so it predicts rollback-free. And you can
  push other players around like this, which is fun.

Remote players:

- Cubic Hermite interpolation with snapshot velocities as tangents: a 20hz
  jump arc renders as an arc, not chords. Falls back to lerp across gaps.
- Capped extrapolation (0.25s of dead-reckoning, then hold), teleport
  detection, and a smoothed server clock with bounded catch-up.

The rest:

- Interest management: the heavy reconcile snapshot goes only to its owner,
  everyone else gets the lean subset. Change-only body snapshots, so parked
  props go silent on the wire.
- Source-style demos: recording taps the raw replicon byte stream, playback
  is a fake network backend the unmodified client runs against. Speed,
  pause, spectator cam.
- A deterministic seeded network conditioner (`--poor-net` is the video
  above), slow-mo, F3 server-truth ghosts, and a wire-size regression test.
- The example game: lag-compensated hitscan with predicted-vs-acked hit
  markers, the owner-authoritative buggy, gravity-gun pickup with predicted
  grab/hold/throw, wasm/WebSocket transport, and a movement showcase map.

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

The pmove does not run at Bevy's cadence, and that is the main gotcha here.
The server may burn through several queued usercmds for one player in a
single tick. The client replays whole bursts of commands after a correction.
Every player is at a different point in time. Inside `player_think`, "now"
means this command, not this frame.

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

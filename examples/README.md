# Examples

Both examples need the [bevy_ahoy fork](https://github.com/wtfdemon/bevy_ahoy/tree/netahoy)
(branch `netahoy`) checked out next to this repository, as `../bevy_ahoy`, as the library itself
does.

| example | what it shows | runs on |
|---|---|---|
| [`minimal`](minimal.rs) | The least a game needs: one file, one binary for both sides | native |
| [`playground`](playground) | A small movement shooter that walks through the library: two weapons, two bots, a moving platform, game state with history, demos, a link conditioner | native and browser |

## minimal

Start here when writing a game. Players run and jump among a floor and four boxes, and each counts
its jumps:

- the game half of a player (`Jumper`, a jump counter) and its one event (`Jumped`), as
  `PlayerGame` and `StepEvent`;
- the server: a player for each client that joins, gone when it leaves;
- the client: the connection, the local player's prediction, a visual for each remote player
  (drawn where the server's snapshots had it), keyboard and mouse input, a camera.

```bash
cargo run --example minimal -- --server
cargo run --example minimal                # a client in a window; start as many as you like
cargo run --example minimal -- --headless  # a client with no window that runs and jumps itself
```

WASD runs, Space jumps, the mouse looks around while the left button is held. A client started
before the server retries until it is up. Each side logs the jumps it steps
(`stepped: player 1 jump 3`): the server every player's, a client its own. A client also logs the
other players' jumps as the server sends them (`from the server: player 2 jump 1`).

| setting | side | |
|---|---|---|
| `--server` | | run the server, not a client |
| `--headless`, `HEADLESS=1` | client | no window and no rendering, so no display either; the player runs in a circle and jumps every 1.5 s |
| `SERVER_ADDR=<ip>:<port>` | both | where the server listens and the client connects (default `127.0.0.1:5000`) |

## playground

A small movement shooter, and a tour of the library: each station on the map shows one thing it
does, with a sign that says what to try. Strafe jumping, bunny hopping and surfing, predicted
rockets with rocket jumps, a lag-compensated hitscan pistol, a platform every player meets where
they saw it, a gate that stops shots as the shooter saw it, and two bots to shoot. Over
WebTransport with a WebSocket fallback, natively and in the browser.

```bash
cargo run --example playground_server
cargo run --example playground_client   # as many as you like
```

### The tour

You spawn facing the range. Each stop, and where its code is:

1. **Prediction.** You move the moment you press: the client steps its own player on each
   command (`PlayerGame::step` on [`Arsenal`](playground/game.rs)) and the server's answer only
   corrects it. F3 draws the server's copy of you beside your own; the HUD says what each
   reconcile did. With `--net=harsh` (or F2) you can watch it hold up on a bad link.
2. **Lag compensation**, at the range ahead. Take the pistol (2) and shoot the **A/D bot**,
   which strafes hard left and right in front of the backstop, or the **strafer** as it hops
   past. The shot is judged against the bot where your screen showed it: at the remote time your
   command carried, a whole tick plus a fraction into the next, on the curve through the bot's
   history. A hit leaves three clay ghosts of its victim: the solid red one where the shot met
   it, and two faint ones where the server had it at the ticks either side. The label gives the
   tick and the fraction, then the server's verdict: the ghost appears the moment your client
   predicts the hit, and the server's copy of the shot confirms it, or says it missed
   ([`client/hit_ghosts.rs`](playground/client/hit_ghosts.rs)).
3. **The shield gate**, across the right half of the range. It rises and falls every 2 s on the
   server's timer and, up, stops rockets and bullets. A shot meets it as the shooter saw it: fire
   while you see it down and the shot passes, though it rose on the server meanwhile.
   - The gate's state goes through the library's history,
     `replicate_with_history::<ShieldGate>()` ([`shared.rs`](playground/shared.rs)), so the
     server records it every tick and both peers hold the same entry at every tick.
   - `Arsenal`'s step reads it through its `Context` at the command's seen time
     (`gate_distance` in [`game.rs`](playground/game.rs)): the pistol's range stops at it, and a
     rocket's sweep judges it at each command's seen time.
   - The client draws it at the same time, the frame's remote render time (`place_gate` in
     [`client/presentation.rs`](playground/client/presentation.rs)).
4. **Rocket jump**, the tower on the right. Rockets (1) are predicted, flight and blast: fire
   at your feet as you jump. A rocket's path is fixed by one raycast when it is fired, so it is
   player state the step can rewind and replay; the blast's push on you is in your step, on
   everyone else in the server's `blast`.
5. **Moving platform**, right of the spawn. A kinematic body the server moves and replicates as
   a `BodySnapshot`; every player's step meets it where that player saw it, and stands on it.
6. **Surf**, the wedge on the left. Slide on a slope too steep to stand on and steer into it;
   hold jump to hop on from its end.

### The bots

The server plays both through `UserCmd`s it queues for them each tick, exactly as a client's
arrive ([`bots.rs`](playground/bots.rs)), so they move by the same step, are replicated the same
and are hit through the same lag compensation as players. A rocket that knocks one off its line
knocks it off, and it finds its way back.

- **The strafer** holds forward and jump and weaves its look either side of where it heads:
  in bevy_ahoy, turning while holding forward in the air is the air strafe, and every radian it
  carves gains speed. It hops round a rounded triangle around the whole map at 13 to 21 m/s,
  sliding off speed on a landing now and then, then hopping on. At that speed the ticks either
  side of a hit are a metre apart, so its hit ghosts show the subtick plainly.
- **The A/D bot** holds A, then D, then A, on the ground across the range, for uneven stretches.

`tests/bots.rs` runs both through the real step and checks they do what they say.

### The code

Two binaries, `playground_server` and `playground_client`, link the `playground` library
crate ([`lib.rs`](playground/lib.rs)): the game ([`game.rs`](playground/game.rs)), the level
([`level.rs`](playground/level.rs)) and the bots live there because replicon builds its
protocol check from the replicated types' names, which must be the same in both binaries. The
server is [`server.rs`](playground/server.rs). The client is [`client/`](playground/client):
the link's loss model and the window title, the players' presentation and their demon models,
the hit ghosts, the rockets, the scene, the camera and the HUD. Both connect through the
library's transport plugins. Every word on screen is
[bevy_msdf_text](https://github.com/wtfdemon/bevy_msdf_text), outlined.

### Transport

The playground connects through the library's transport plugins (the `transport-client` and
`transport-server` features, [`src/transport`](../src/transport)): the server adds
`NetAhoyTransportServerPlugin`, the client `NetAhoyTransportClientPlugin`, and neither opens a
session itself. The client tries WebTransport first and falls back to WebSocket, by the rules in
[`src/transport/rules.rs`](../src/transport/rules.rs). A first handshake that fails or hangs for
5 s means WebSocket for the rest of the run. When UDP alone dies mid-game (2 s into a silence the
same run of the server answers its offer over HTTP with its tick moved on, and WebTransport hears
nothing for a second more), the reconnect goes over WebSocket, and the client keeps that
WebSocket session until it ends (no session ever moves to the other transport); a reconnect a
minute or more after WebTransport dropped tries it again. A whole link going down, a restarted
server, a frozen one (its tick stands still) or a kick goes back to WebTransport. WebTransport's
datagrams don't stall on a loss the way TCP does, and each command packet repeats the commands
the server hasn't acked, so a lost one is filled by the next (the session is tagged
`LinkLossModel::Datagram`; a WebSocket's is `Stream`, which sends only the newest, unless the
link conditioner drops packets for good: [`client/connect.rs`](playground/client/connect.rs)).
The server makes its own self-signed certificate and answers its SHA-256 hash, its UDP port, a
number for this run of the server and its tick as JSON on a small HTTP endpoint; the client
trusts the certificate by that hash, so there is no certificate to install. Browsers allow that
only for one valid at most 14 days: each is valid 14 days from an hour before it was made (for
clients whose clock runs behind), and a client asking when it has a day left makes a new one
([`certificate.rs`](../src/transport/server/certificate.rs)). Handshakes get the new one a
minute later, and meanwhile the offer lists both, so a client that asked just before still meets
the certificate it was told of ([`rotation.rs`](../src/transport/server/rotation.rs)). Open
sessions keep the one they started on, so a server can run for as long as it likes.
`TRANSPORT=ws` or `wt` (`?transport=ws` or `wt` on the page) pins one transport, with no
fallback.

### Native

```bash
cargo run --example playground_server
cargo run --example playground_client  # as many as you like
```

Or with [just](https://github.com/casey/just):

```bash
just play                 # the server and 2 client windows on a harsh link
just play 3 transatlantic # 3 clients, another bevy_link_conditioner preset
just play 2 ""            # a perfect link
just record               # the first client records my.ahoydem
just playback             # play it back, no server
```

| key | | key | |
|---|---|---|---|
| WASD | move | Space | jump (hold to bunnyhop) |
| Ctrl, C | crouch | right mouse, F | fire |
| 1 | rockets | 2 | pistol |
| left click | capture the cursor | Esc | release it |
| F2 | network panel | F3 | the server's players as ghosts |

In demo playback, Space pauses and the arrow keys change the speed.

### Browser

```bash
just web                  # http://localhost:8080; open more tabs for more players
```

`just web` builds `playground_client` for `wasm32-unknown-unknown` with
[`scripts/build_wasm_client.sh`](../scripts/build_wasm_client.sh) (it installs the matching
`wasm-bindgen-cli` under `target/` if needed), runs `playground_server` natively and serves `web/`.
`just web transatlantic` conditions every player's link from the server. A page served over
plain http connects to its own host on port 5000, and asks port 5002 for the WebTransport offer;
one served over https connects to `wss://` at `ws` beside the page, for a server behind the same
proxy, and asks `wt` beside the page. Either way the WebTransport session goes to the page's host
on the offer's UDP port: QUIC doesn't go through an HTTP proxy. A page on `localhost` sends it to
127.0.0.1, since Chrome dials QUIC to `localhost` on `::1` alone.

The browser client's clock is the frame's vsync time (`document.timeline`), not the moment its
update starts (`vsync_clock` in [`client/main.rs`](playground/client/main.rs)): each frame then
advances everything by what the screen advanced, and the remote players and bots glide evenly.

### Settings

Environment variables natively, page query parameters in a browser:

| setting | side | |
|---|---|---|
| `TICK_HZ=<n>`, `?tick_hz=<n>` | both | ticks a second, 10 to 1000 (default 20). Give the server and every client the same rate, or they refuse each other |
| `AUTOPILOT=1`, `?autopilot` | client | the client plays by itself, the same script every run: 8 s with a rocket every 2 s, then 8 s with a pistol shot every 0.5 s at the nearer bot, in turn |
| `HEADLESS=1`, `--headless` | client | no window and a 60 Hz loop. It still renders, so it needs a display (the continuity harness gives each client an Xvfb) |
| `TRANSPORT=auto\|wt\|ws`, `?transport=` | client | WebTransport first with the WebSocket fallback (`auto`, the default), or only one of them |
| `SERVER_URL=<ws url>` | client | where to connect (default `ws://127.0.0.1:5000`); the WebTransport session goes to its host |
| `WT_HASH_URL=<http url>` | client | where to ask for the WebTransport offer (default port 5002 on `SERVER_URL`'s host) |
| `SERVER_BIND_ADDR=<ip>:<port>` | server | where the WebSocket listens (default `127.0.0.1:5000`); WebTransport listens on the same IP |
| `WT_PORT=<port>` | server | the WebTransport UDP port (default 5001) |
| `WT_BIND_ADDR=<ip>:<port>` | server | where WebTransport listens instead of `SERVER_BIND_ADDR`'s IP and `WT_PORT`, e.g. `[::]:5001` for every interface while the WebSocket sits on loopback behind a proxy |
| `WT_HASH_ADDR=<ip>:<port>` | server | where to answer the WebTransport offer (default `127.0.0.1:5002`) |
| `NETAHOY_ROLLBACK=0` | server | server input rollback off |
| `SERVER_LOOP_HZ=<n>` | server | a fixed-rate loop instead of one frame per tick |
| `CLIENT_LOOP_HZ=<n>` | client | the headless loop's rate (default 60) |

And on the command line:

| flag | side | |
|---|---|---|
| `--net=<preset or spec>` | both | condition the link ([bevy_link_conditioner](https://github.com/Smoked-Software/bevy_link_conditioner); also `NET_CONDITION`). On a client it conditions that client's link, and F2 tunes it live; on the server, every player's. Use one side or the other, not both |
| `--slowmo=<factor>` | both | slow everything down (`--slowmo` alone: 0.1) |
| `--show-ghosts` | client | draw the server's players from the start (F3) |
| `--record <file>` | client | record a demo |
| `--play <file>` | client | play a demo back, no server |

The presets are `lan`, `transatlantic`, `mobile`, `bursty`, `harsh`, `brutal` and `stall`; a spec
is a preset and/or `key=value` items, e.g. `--net=latency=40ms,jitter=8ms,loss=2%`.

### Bad networks, measured

`scripts/continuity-linux.sh` runs a server and N headless autopilot clients in their own Linux
user and network namespace, with real kernel netem on the link (profiles `clean lan transatlantic
bursty harsh clumsy15 clumsy5 brutal stall`), rootless, then `scripts/continuity-analyze.py` on
their logs. It needs a software Vulkan driver (lavapipe), as the [README](../README.md) says.

```bash
BUILD=1 scripts/continuity-linux.sh 2 60 harsh                # players, seconds, profile
TRANSPORT=wt BUILD=1 scripts/continuity-linux.sh 2 60 harsh   # over WebTransport
```

The clients run over WebSocket unless `TRANSPORT=wt`, and a run whose clients connected over
anything else fails; the analyzer lists each client's transports. `TRANSPORT=auto` lets the clients
choose, as they do by default, and fails the run if any session went over WebSocket: the harness
never blocks UDP alone, so with a `HOOK=` script that takes the whole link down (`link_down`, then
`link_up`), every client must come back on WebTransport.

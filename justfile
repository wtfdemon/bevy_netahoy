server_name := "playground_server"
client_name := "playground_client"
port := "5000"

# The example server and `players` client windows, every client's link conditioned (`net`: a bevy_link_conditioner preset or spec, "" for none; F2 tunes it live).
play players="2" net="harsh" slowmo="1.0" show_ghosts="false":
    @just _local "{{players}}" "{{net}}" "{{slowmo}}" "{{show_ghosts}}" ""

# Like `play`, and the first client records a demo.
record demo="my.ahoydem" players="1" net="harsh" slowmo="1.0" show_ghosts="false":
    @just _local "{{players}}" "{{net}}" "{{slowmo}}" "{{show_ghosts}}" "{{demo}}"

# Play a recorded demo back. No server needed.
playback demo="my.ahoydem" show_ghosts="false":
    #!/usr/bin/env bash
    set -euo pipefail
    ARGS=(--play "{{demo}}")
    [[ "{{show_ghosts}}" == "true" ]] && ARGS+=(--show-ghosts)
    cargo run --example {{client_name}} -- "${ARGS[@]}"

# Build the browser client, run the server natively and serve the page on http://localhost:<web_port>. Open more tabs for more players.
web net="" web_port="8080":
    #!/usr/bin/env bash
    set -euo pipefail
    ARGS=()
    [[ -n "{{net}}" ]] && ARGS+=("--net={{net}}")
    ./scripts/build_wasm_client.sh
    cargo build --example {{server_name}}
    ./target/debug/examples/{{server_name}} "${ARGS[@]}" &
    SERVER_PID=$!
    trap 'kill $SERVER_PID 2>/dev/null || true' EXIT INT TERM
    echo "http://localhost:{{web_port}}"
    cd web && python3 -m http.server "{{web_port}}"

# The test suite.
test:
    cargo nextest run || cargo test

_local players net slowmo show_ghosts record_path:
    #!/usr/bin/env bash
    set -euo pipefail
    SERVER_ARGS=("--slowmo={{slowmo}}")
    CLIENT_ARGS=("--slowmo={{slowmo}}")
    [[ -n "{{net}}" ]] && CLIENT_ARGS+=("--net={{net}}")
    [[ "{{show_ghosts}}" == "true" ]] && CLIENT_ARGS+=(--show-ghosts)

    cargo build --example {{server_name}} --example {{client_name}}
    BIN=./target/debug/examples

    PIDS=()
    trap 'kill "${PIDS[@]}" 2>/dev/null || true; wait 2>/dev/null || true' EXIT INT TERM

    "$BIN/{{server_name}}" "${SERVER_ARGS[@]}" &
    PIDS+=("$!")
    for _ in $(seq 1 30); do
        (exec 3<>/dev/tcp/127.0.0.1/{{port}}) 2>/dev/null && break
        sleep 0.5
    done

    for id in $(seq 1 "{{players}}"); do
        ARGS=("${CLIENT_ARGS[@]}")
        [[ "$id" == 1 && -n "{{record_path}}" ]] && ARGS+=(--record "{{record_path}}")
        "$BIN/{{client_name}}" "${ARGS[@]}" &
        PIDS+=("$!")
        sleep 1
    done
    wait

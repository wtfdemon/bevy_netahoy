win_target := "x86_64-pc-windows-gnu"
stage_dir := "/mnt/e/bevy-netahoy-dev"
server_name := "ahoy_server"
client_name := "ahoy_client"
port := "5000"
web_port := "8080"

# Build the websocket server/client examples for Windows from WSL2 and run local player windows with poor networking.
win-dev players="2" slowmo="1.0" show_ghosts="false":
    @just _win-local "{{players}}" "{{slowmo}}" "{{show_ghosts}}" "true"

# Build the websocket server/client examples for Windows from WSL2 and run local player windows without simulated ping/loss.
win-perfect players="2" slowmo="1.0" show_ghosts="false":
    @just _win-local "{{players}}" "{{slowmo}}" "{{show_ghosts}}" "false"

_win-local players slowmo show_ghosts poor_net:
    #!/usr/bin/env bash
    set -euo pipefail

    WIN_TARGET="{{win_target}}"
    STAGE_DIR="{{stage_dir}}"
    SERVER_NAME="{{server_name}}"
    CLIENT_NAME="{{client_name}}"
    PORT="{{port}}"
    SERVER_EXE="./target/${WIN_TARGET}/win-dev/examples/${SERVER_NAME}.exe"
    CLIENT_EXE="./target/${WIN_TARGET}/win-dev/examples/${CLIENT_NAME}.exe"
    PLAYERS="{{players}}"
    SLOWMO="{{slowmo}}"
    SHOW_GHOSTS="{{show_ghosts}}"
    POOR_NET="{{poor_net}}"
    SERVER_ARGS=()
    CLIENT_ARGS=()
    NET_LABEL="normal network conditions"
    PIDS=()

    command -v cmd.exe >/dev/null 2>&1 || { echo "cmd.exe not found; run this from WSL2."; exit 1; }
    [[ "${PLAYERS}" =~ ^[0-9]+$ ]] && (( PLAYERS > 0 )) || { echo "players must be a positive integer"; exit 2; }
    [[ "${SLOWMO}" =~ ^[0-9]+([.][0-9]+)?$ ]] || { echo "slowmo must be a positive number"; exit 2; }
    [[ "${SHOW_GHOSTS}" == "true" || "${SHOW_GHOSTS}" == "false" ]] || { echo "show_ghosts must be true or false"; exit 2; }

    if [[ "${POOR_NET}" == "true" ]]; then
        SERVER_ARGS+=(--poor-net)
        CLIENT_ARGS+=(--poor-net)
        NET_LABEL="poor network conditions"
    fi
    if [[ "${SLOWMO}" != "1" && "${SLOWMO}" != "1.0" ]]; then
        SERVER_ARGS+=(--slowmo "${SLOWMO}")
        CLIENT_ARGS+=(--slowmo "${SLOWMO}")
    fi
    if [[ "${SHOW_GHOSTS}" == "true" ]]; then
        CLIENT_ARGS+=(--show-ghosts)
    fi

    cmd.exe /C "taskkill /IM ${SERVER_NAME}.exe /F >NUL 2>&1" || true
    cmd.exe /C "taskkill /IM ${CLIENT_NAME}.exe /F >NUL 2>&1" || true
    sleep 0.5

    echo "Building ${SERVER_NAME} and ${CLIENT_NAME} for Windows..."
    cargo build --example "${SERVER_NAME}" --example "${CLIENT_NAME}" --target "${WIN_TARGET}" --profile win-dev

    echo "Staging to E:\\bevy-netahoy-dev..."
    mkdir -p "${STAGE_DIR}"
    cp "${SERVER_EXE}" "${STAGE_DIR}/${SERVER_NAME}.exe"
    cp "${CLIENT_EXE}" "${STAGE_DIR}/${CLIENT_NAME}.exe"

    cleanup() {
        echo ""
        echo "Shutting down..."
        for pid in "${PIDS[@]}"; do
            kill "${pid}" 2>/dev/null || true
        done
        cmd.exe /C "taskkill /IM ${SERVER_NAME}.exe /F >NUL 2>&1" || true
        cmd.exe /C "taskkill /IM ${CLIENT_NAME}.exe /F >NUL 2>&1" || true
        wait 2>/dev/null || true
    }
    trap cleanup EXIT INT TERM

    echo "Starting server with ${NET_LABEL} and time scale ${SLOWMO}x..."
    (cd "${STAGE_DIR}" && "./${SERVER_NAME}.exe" "${SERVER_ARGS[@]}") &
    PIDS+=("$!")

    echo "Waiting for server to be ready..."
    for _ in $(seq 1 30); do
        if bash -lc "exec 3<>/dev/tcp/127.0.0.1/${PORT}" 2>/dev/null; then
            break
        fi
        sleep 0.5
    done
    sleep 1

    for id in $(seq 1 "${PLAYERS}"); do
        echo "Starting client ${id} with ${NET_LABEL}, time scale ${SLOWMO}x, show ghosts ${SHOW_GHOSTS}..."
        (cd "${STAGE_DIR}" && "./${CLIENT_NAME}.exe" "${CLIENT_ARGS[@]}") &
        PIDS+=("$!")
        sleep 1
    done

    wait

# Build the browser client, run a local websocket server, and serve the web shell.
win-web poor_net="false" web_port="8080":
    #!/usr/bin/env bash
    set -euo pipefail

    WIN_TARGET="{{win_target}}"
    STAGE_DIR="{{stage_dir}}"
    SERVER_NAME="{{server_name}}"
    PORT="{{port}}"
    WEB_PORT="{{web_port}}"
    POOR_NET="{{poor_net}}"
    WINDOWS_SERVER_EXE="./target/${WIN_TARGET}/win-dev/examples/${SERVER_NAME}.exe"
    NATIVE_SERVER_BIN="./target/debug/examples/${SERVER_NAME}"
    SERVER_ARGS=()
    SERVER_PID=""
    USE_WINDOWS_SERVER="false"

    command -v python3 >/dev/null 2>&1 || { echo "python3 not found"; exit 1; }
    if command -v cmd.exe >/dev/null 2>&1; then
        USE_WINDOWS_SERVER="true"
    fi

    if [[ "${POOR_NET}" == "true" ]]; then
        SERVER_ARGS+=(--poor-net)
    elif [[ "${POOR_NET}" != "false" ]]; then
        echo "poor_net must be true or false"
        exit 2
    fi

    windows_cmd() {
        if [[ -d /mnt/c/Windows/System32 ]]; then
            (cd /mnt/c/Windows/System32 && cmd.exe /C "$1")
        else
            cmd.exe /C "$1"
        fi
    }

    cleanup() {
        echo ""
        echo "Shutting down..."
        if [[ -n "${SERVER_PID}" ]]; then
            kill "${SERVER_PID}" 2>/dev/null || true
            wait "${SERVER_PID}" 2>/dev/null || true
        fi
        if [[ "${USE_WINDOWS_SERVER}" == "true" ]]; then
            windows_cmd "taskkill /IM ${SERVER_NAME}.exe /F >NUL 2>&1" || true
        fi
    }
    trap cleanup EXIT INT TERM

    ./scripts/build_wasm_client.sh

    if [[ "${USE_WINDOWS_SERVER}" == "true" ]]; then
        windows_cmd "taskkill /IM ${SERVER_NAME}.exe /F >NUL 2>&1" || true
        echo "Building ${SERVER_NAME} for Windows..."
        cargo build --example "${SERVER_NAME}" --target "${WIN_TARGET}" --profile win-dev
        echo "Staging to E:\\bevy-netahoy-dev..."
        mkdir -p "${STAGE_DIR}"
        cp "${WINDOWS_SERVER_EXE}" "${STAGE_DIR}/${SERVER_NAME}.exe"
        echo "Starting Windows websocket server on ws://127.0.0.1:${PORT}..."
        (cd "${STAGE_DIR}" && "./${SERVER_NAME}.exe" "${SERVER_ARGS[@]}") &
    else
        echo "Building ${SERVER_NAME}..."
        cargo build --example "${SERVER_NAME}"
        echo "Starting websocket server on ws://127.0.0.1:${PORT}..."
        "${NATIVE_SERVER_BIN}" "${SERVER_ARGS[@]}" &
    fi
    SERVER_PID="$!"

    server_ready() {
        if [[ "${USE_WINDOWS_SERVER}" == "true" ]]; then
            powershell.exe -NoProfile -Command "\$client = New-Object Net.Sockets.TcpClient; try { \$client.Connect('127.0.0.1', ${PORT}); \$client.Close(); exit 0 } catch { exit 1 }" >/dev/null 2>&1
        else
            bash -lc "exec 3<>/dev/tcp/127.0.0.1/${PORT}" 2>/dev/null
        fi
    }

    echo "Waiting for server to be ready..."
    for _ in $(seq 1 40); do
        if server_ready; then
            break
        fi
        if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
            wait "${SERVER_PID}" || true
            echo "server exited before opening port ${PORT}"
            exit 1
        fi
        sleep 0.25
    done

    if ! server_ready; then
        echo "server did not open port ${PORT}"
        exit 1
    fi

    echo ""
    echo "==================================="
    echo "  http://localhost:${WEB_PORT}"
    echo "==================================="
    echo ""

    cd web && python3 -m http.server "${WEB_PORT}"

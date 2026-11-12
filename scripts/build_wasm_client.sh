#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

OUT_DIR="web"
WASM_IN="target/wasm32-unknown-unknown/release/examples/playground_client.wasm"
WASM_OUT="${OUT_DIR}/playground_client_bg.wasm"

WASM_BINDGEN_VERSION=$(
    awk '
        $0 == "[[package]]" { in_package = 0 }
        $0 == "name = \"wasm-bindgen\"" { in_package = 1 }
        in_package && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' Cargo.lock
)

if [[ -z "${WASM_BINDGEN_VERSION}" ]]; then
    echo "could not determine wasm-bindgen version from Cargo.lock"
    exit 1
fi

LOCAL_BINDGEN="target/wasm-bindgen-cli/${WASM_BINDGEN_VERSION}/bin/wasm-bindgen"

if [[ -n "${WASM_BINDGEN:-}" ]]; then
    BINDGEN_BIN="${WASM_BINDGEN}"
elif command -v wasm-bindgen >/dev/null 2>&1 \
    && [[ "$(wasm-bindgen --version | awk '{ print $2 }')" == "${WASM_BINDGEN_VERSION}" ]]; then
    BINDGEN_BIN="wasm-bindgen"
else
    BINDGEN_BIN="${LOCAL_BINDGEN}"
fi

if [[ "${BINDGEN_BIN}" == "${LOCAL_BINDGEN}" && ! -x "${LOCAL_BINDGEN}" ]]; then
    echo "Installing wasm-bindgen-cli ${WASM_BINDGEN_VERSION} locally..."
    cargo install \
        --root "target/wasm-bindgen-cli/${WASM_BINDGEN_VERSION}" \
        --version "${WASM_BINDGEN_VERSION}" \
        wasm-bindgen-cli
fi

mkdir -p "${OUT_DIR}"

echo "Building playground_client for WASM..."
cargo build --release --target wasm32-unknown-unknown --example playground_client

echo "Generating JS bindings..."
"${BINDGEN_BIN}" \
    --out-dir "${OUT_DIR}" \
    --target web \
    --no-typescript \
    "${WASM_IN}"

if [[ "${SKIP_WASM_OPT:-0}" == "1" ]]; then
    echo "Skipping wasm-opt (SKIP_WASM_OPT=1)"
elif command -v wasm-opt >/dev/null 2>&1; then
    echo "Optimizing WASM..."
    wasm-opt --enable-bulk-memory --enable-reference-types --enable-sign-ext \
        --enable-multivalue --enable-mutable-globals --enable-nontrapping-float-to-int \
        -Os "${WASM_OUT}" -o "${WASM_OUT}"
else
    echo "wasm-opt not found; skipping optimization"
fi

RAW_BYTES=$(wc -c < "${WASM_OUT}" | tr -d ' ')
echo "WASM size: ${RAW_BYTES} bytes"
echo "WASM build complete: ${OUT_DIR}/"

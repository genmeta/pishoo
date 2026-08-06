#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PISHOO_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
GENMETA_ROOT="$(cd "$PISHOO_REPO/.." && pwd)"
for TEST_COMMAND in cargo; do
    if ! command -v "$TEST_COMMAND" >/dev/null 2>&1; then
        echo "missing required command: $TEST_COMMAND" >&2
        exit 1
    fi
done

CARGO_PATCH_ARGS=(
    --config "patch.crates-io.dhttp.path=\"$GENMETA_ROOT/dhttp/dhttp\""
    --config "patch.crates-io.dhttp-access.path=\"$GENMETA_ROOT/dhttp/access\""
    --config "patch.crates-io.dhttp-identity.path=\"$GENMETA_ROOT/dhttp/identity\""
    --config "patch.crates-io.dyns.path=\"$GENMETA_ROOT/ddns\""
    --config "patch.crates-io.dshell.path=\"$GENMETA_ROOT/dssh\""
    --config "patch.crates-io.h3x.path=\"$GENMETA_ROOT/h3x\""
    --config "patch.crates-io.dquic.path=\"$GENMETA_ROOT/dquic/dquic\""
    --config "patch.crates-io.qbase.path=\"$GENMETA_ROOT/dquic/qbase\""
    --config "patch.crates-io.qcongestion.path=\"$GENMETA_ROOT/dquic/qcongestion\""
    --config "patch.crates-io.qconnection.path=\"$GENMETA_ROOT/dquic/qconnection\""
    --config "patch.crates-io.qdatagram.path=\"$GENMETA_ROOT/dquic/qdatagram\""
    --config "patch.crates-io.qevent.path=\"$GENMETA_ROOT/dquic/qevent\""
    --config "patch.crates-io.qinterface.path=\"$GENMETA_ROOT/dquic/qinterface\""
    --config "patch.crates-io.qmacro.path=\"$GENMETA_ROOT/dquic/qmacro\""
    --config "patch.crates-io.qrecovery.path=\"$GENMETA_ROOT/dquic/qrecovery\""
    --config "patch.crates-io.qresolve.path=\"$GENMETA_ROOT/dquic/qresolve\""
    --config "patch.crates-io.qtraversal.path=\"$GENMETA_ROOT/dquic/qtraversal\""
    --config "patch.crates-io.qudp.path=\"$GENMETA_ROOT/dquic/qudp\""
)

cd "$PISHOO_REPO"

echo "Running identity hot-reload integration test..."
cargo test --locked \
    -p pishoo \
    --test identity_hot_reload \
    --features sshd \
    "${CARGO_PATCH_ARGS[@]}" \
    -- \
    --nocapture

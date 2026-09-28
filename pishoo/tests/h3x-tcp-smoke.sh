#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
DHTTP_HOME=$(mktemp -d "${TMPDIR:-/tmp}/pishoo-h3x-smoke.XXXXXX")
DHTTP_TCP_MOCK_PORTS=${DHTTP_TCP_MOCK_PORTS:-demo.dhttp.net=18472}
PISHOO_PROXY_PORT=${PISHOO_PROXY_PORT:-18474}
export DHTTP_HOME DHTTP_TCP_MOCK_PORTS PISHOO_PROXY_PORT
cargo build --locked --offline -p pishoo --features tcp-mock --bin pishoo --example pishoo-client --example setup-tcp-demo
target/debug/examples/setup-tcp-demo
python3 pishoo/tests/support/local-upstream.py "$PISHOO_PROXY_PORT" "$DHTTP_HOME/demo/file" >/dev/null 2>&1 &
proxy_pid=$!
target/debug/pishoo &
server_pid=$!
cleanup() {
    kill "$server_pid" 2>/dev/null || true
    kill "$proxy_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    wait "$proxy_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM
ready=0
attempt=0
while [ "$attempt" -lt 200 ]; do
    if python3 -c 'import socket,sys; s=socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.1); s.close()' "$PISHOO_PROXY_PORT" >/dev/null 2>&1; then
        ready=1
        break
    fi
    if ! kill -0 "$proxy_pid" 2>/dev/null; then
        echo 'Local HTTP proxy upstream exited before accepting connections' >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ "$ready" -ne 1 ]; then
    echo 'Local HTTP proxy upstream did not become ready' >&2
    exit 1
fi
ready=0
attempt=0
while [ "$attempt" -lt 200 ]; do
    if target/debug/examples/pishoo-client get /file/hello.txt >/dev/null 2>&1; then
        ready=1
        break
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
        echo 'Pishoo server exited before the h3x/TCP client connected' >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ "$ready" -ne 1 ]; then
    echo 'Pishoo h3x/TCP listener did not become ready' >&2
    exit 1
fi
target/debug/examples/pishoo-client smoke

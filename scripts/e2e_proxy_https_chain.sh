#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# 端到端链路使用的本地端口：
# curl -> forward(FORWARD_PORT) -> pishoo(PISHOO_PORT) -> https upstream(UPSTREAM_PORT)
PISHOO_PORT=${PISHOO_PORT:-15378}
FORWARD_PORT=${FORWARD_PORT:-15379}
UPSTREAM_PORT=${UPSTREAM_PORT:-18443}

# 通过 forward 访问的目标域名；该域名由 pishoo 的 server_name 接住
TARGET_HOST=${TARGET_HOST:-test.genmeta.net}

# 所有脚本生成物默认都放在仓库根目录下的 tmp/ 目录中，避免散落到系统临时目录。
TMP_ROOT=${TMP_ROOT:-$REPO_ROOT/tmp}

if [[ -n "${TMP_DIR:-}" ]]; then
    mkdir -p "$TMP_DIR"
else
    mkdir -p "$TMP_ROOT"
    TMP_DIR=$(mktemp -d "$TMP_ROOT/gateway-e2e-https.XXXXXX")
fi

# 单独输出的结果文件，默认也放到仓库根目录下的 tmp/ 目录中，便于 CI 或人工收集。
RESULT_DIR=${RESULT_DIR:-$TMP_ROOT/e2e-results}
UPSTREAM_CLIENT_CERT_INFO_FILE=${UPSTREAM_CLIENT_CERT_INFO_FILE:-$RESULT_DIR/upstream-client-cert.txt}

# 为 1 时保留临时目录，方便排查；否则脚本成功后自动清理
KEEP_E2E_ARTIFACTS=${KEEP_E2E_ARTIFACTS:-0}

# 运行时日志级别
RUST_LOG_VALUE=${RUST_LOG:-info,services=info,dns=info,forward_proxy=debug,reverse_proxy=debug,upstream_tls=debug}

# 三个关键进程的日志文件
HTTPS_SERVER_LOG="$TMP_DIR/https-upstream.log"
PISHOO_LOG="$TMP_DIR/pishoo.log"
FORWARD_LOG="$TMP_DIR/forward.log"

# 动态写出的配置文件与运行期数据文件
PISHOO_CONF="$TMP_DIR/pishoo.conf"
FORWARD_CONF="$TMP_DIR/forward.conf"
RULES_DB="$TMP_DIR/rules.db"
GENMETA_HOME_DIR="$TMP_DIR/genmeta-home"

# 本脚本动态生成的 CA / 服务端 / 客户端证书材料
CA_CERT="$TMP_DIR/upstream-ca.crt"
CA_KEY="$TMP_DIR/upstream-ca.key"
SERVER_CERT="$TMP_DIR/upstream-server.crt"
SERVER_KEY="$TMP_DIR/upstream-server.key"
SERVER_CSR="$TMP_DIR/upstream-server.csr"
SERVER_EXT="$TMP_DIR/upstream-server.ext"
CLIENT_CERT="$TMP_DIR/upstream-client.crt"
CLIENT_KEY="$TMP_DIR/upstream-client.key"
CLIENT_CSR="$TMP_DIR/upstream-client.csr"
CLIENT_EXT="$TMP_DIR/upstream-client.ext"
PROBE_CLIENT_CERT="$TMP_DIR/direct-probe-client.crt"
PROBE_CLIENT_KEY="$TMP_DIR/direct-probe-client.key"
PROBE_CLIENT_CSR="$TMP_DIR/direct-probe-client.csr"

# pishoo 自身对外提供服务时使用的现有测试证书
PISHOO_CERT="$REPO_ROOT/keychain/test.genmeta.net/test.genmeta.net.pem"
PISHOO_KEY="$REPO_ROOT/keychain/test.genmeta.net/test.genmeta.net.key"

# 本地 HTTPS upstream 小工具的工程与二进制位置
HTTPS_SERVER_MANIFEST="$REPO_ROOT/tools/https-upstream-server/Cargo.toml"
PISHOO_BIN="$REPO_ROOT/target/debug/pishoo"
FORWARD_BIN="$REPO_ROOT/target/debug/examples/forward"
HTTPS_SERVER_BIN="$REPO_ROOT/tools/https-upstream-server/target/debug/https-upstream-server"

# 启动后的后台进程信息，用于退出时统一清理
PIDS=()
PROCESS_NAMES=()
PROCESS_LOGS=()

info() {
    printf '[信息] %s\n' "$*"
}

error() {
    printf '[错误] %s\n' "$*" >&2
}

cleanup() {
    local status=$?

    # 无论成功失败，都尽量结束脚本拉起的后台进程，避免残留。
    for pid in "${PIDS[@]:-}"; do
        if kill -0 "$pid" >/dev/null 2>&1; then
            kill "$pid" >/dev/null 2>&1 || true
        fi
    done

    for _ in $(seq 1 20); do
        local any_running=0
        for pid in "${PIDS[@]:-}"; do
            if kill -0 "$pid" >/dev/null 2>&1; then
                any_running=1
                break
            fi
        done
        [[ "$any_running" -eq 0 ]] && break
        sleep 0.2
    done

    for pid in "${PIDS[@]:-}"; do
        if kill -0 "$pid" >/dev/null 2>&1; then
            kill -9 "$pid" >/dev/null 2>&1 || true
        fi
        wait "$pid" >/dev/null 2>&1 || true
    done

    if [[ "$status" -ne 0 ]]; then
        printf '\n[失败] 端到端链路验证失败，日志保留在：%s\n' "$TMP_DIR" >&2
        for log_file in "$HTTPS_SERVER_LOG" "$PISHOO_LOG" "$FORWARD_LOG"; do
            if [[ -f "$log_file" ]]; then
                printf '\n===== %s =====\n' "$log_file" >&2
                cat "$log_file" >&2
            fi
        done
    elif [[ "$KEEP_E2E_ARTIFACTS" != "1" ]]; then
        rm -rf "$TMP_DIR"
    else
        info "按要求保留调试产物目录：$TMP_DIR"
    fi

    exit "$status"
}

trap cleanup EXIT

build_targets() {
    # 先编译，再启动；这样轮询阶段不会因为后台进程仍在编译而长时间等待。
    info '开始编译 pishoo、forward，以及本地 HTTPS 测试服务'
    cargo build -p pishoo -p gateway --example forward
    cargo build --manifest-path "$HTTPS_SERVER_MANIFEST"
    info '相关二进制编译完成'
}

ensure_processes_running() {
    local index

    # 轮询阶段随时检查后台进程是否已异常退出，避免脚本无意义等待。
    for index in "${!PIDS[@]}"; do
        if ! kill -0 "${PIDS[$index]}" >/dev/null 2>&1; then
            printf '[错误] 进程提前退出：%s，日志文件：%s\n' \
                "${PROCESS_NAMES[$index]}" \
                "${PROCESS_LOGS[$index]}" >&2
            return 1
        fi
    done
}

generate_certificates() {
    # 这里动态生成一套仅供本次 e2e 使用的 CA、服务端证书、客户端证书：
    # - 本地 HTTPS upstream 使用服务端证书
    # - pishoo 访问 upstream 时使用客户端证书
    # - upstream 用 CA 校验客户端证书，形成 mTLS
    mkdir -p "$GENMETA_HOME_DIR"

    cat > "$SERVER_EXT" <<'EOF'
subjectAltName=DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
EOF

    cat > "$CLIENT_EXT" <<'EOF'
extendedKeyUsage=clientAuth
EOF

    openssl req -x509 -newkey rsa:2048 -sha256 -nodes \
        -days 365 \
        -subj "/CN=Gateway E2E Upstream CA" \
        -keyout "$CA_KEY" \
        -out "$CA_CERT" >/dev/null 2>&1

    openssl req -newkey rsa:2048 -sha256 -nodes \
        -subj "/CN=localhost" \
        -keyout "$SERVER_KEY" \
        -out "$SERVER_CSR" >/dev/null 2>&1

    openssl x509 -req \
        -in "$SERVER_CSR" \
        -CA "$CA_CERT" \
        -CAkey "$CA_KEY" \
        -CAcreateserial \
        -days 365 \
        -sha256 \
        -extfile "$SERVER_EXT" \
        -out "$SERVER_CERT" >/dev/null 2>&1

    openssl req -newkey rsa:2048 -sha256 -nodes \
        -subj "/CN=proxy-client" \
        -keyout "$CLIENT_KEY" \
        -out "$CLIENT_CSR" >/dev/null 2>&1

    openssl x509 -req \
        -in "$CLIENT_CSR" \
        -CA "$CA_CERT" \
        -CAkey "$CA_KEY" \
        -CAcreateserial \
        -days 365 \
        -sha256 \
        -extfile "$CLIENT_EXT" \
        -out "$CLIENT_CERT" >/dev/null 2>&1

    openssl req -newkey rsa:2048 -sha256 -nodes \
        -subj "/CN=direct-probe-client" \
        -keyout "$PROBE_CLIENT_KEY" \
        -out "$PROBE_CLIENT_CSR" >/dev/null 2>&1

    openssl x509 -req \
        -in "$PROBE_CLIENT_CSR" \
        -CA "$CA_CERT" \
        -CAkey "$CA_KEY" \
        -CAcreateserial \
        -days 365 \
        -sha256 \
        -extfile "$CLIENT_EXT" \
        -out "$PROBE_CLIENT_CERT" >/dev/null 2>&1

    info '已生成本地 HTTPS upstream 所需的 CA、服务端证书，以及测试用客户端证书'
}

write_configs() {
    local access_rules_uri="sqlite:///${RULES_DB#/}?mode=rwc"

    # pishoo 负责承接 test.genmeta.net，并把请求反代到本地 HTTPS upstream。
    # 这里显式配置：
    # - proxy_ssl_certificate / proxy_ssl_certificate_key：作为 upstream mTLS 客户端身份
    # - proxy_ssl_trusted_certificate：用于校验 upstream 服务端证书
    cat > "$PISHOO_CONF" <<EOF
pishoo {
    pid $TMP_DIR/pishoo.pid;
    access_rules $access_rules_uri;

    server {
        listen all $PISHOO_PORT;
        server_name $TARGET_HOST;

        ssl_certificate $PISHOO_CERT;
        ssl_certificate_key $PISHOO_KEY;

        location / {
            proxy_pass https://localhost:$UPSTREAM_PORT/;
            proxy_ssl_certificate $CLIENT_CERT;
            proxy_ssl_certificate_key $CLIENT_KEY;
            proxy_ssl_trusted_certificate $CA_CERT;
        }
    }
}
EOF

    # forward 只允许该目标域名走代理链路。
    cat > "$FORWARD_CONF" <<EOF
pishoo {
    proxy {
        listen 127.0.0.1:$FORWARD_PORT;
        allow $TARGET_HOST;
    }
}
EOF

    info '已写出 pishoo 与 forward 的临时配置文件'
}

start_processes() {
    info "启动本地 HTTPS upstream 服务，监听端口：$UPSTREAM_PORT"
    "$HTTPS_SERVER_BIN" \
        --addr "127.0.0.1:$UPSTREAM_PORT" \
        --server-cert "$SERVER_CERT" \
        --server-key "$SERVER_KEY" \
        --client-ca "$CA_CERT" \
        --response-text "upstream-https-ok" >"$HTTPS_SERVER_LOG" 2>&1 &
    PIDS+=("$!")
    PROCESS_NAMES+=("https-upstream-server")
    PROCESS_LOGS+=("$HTTPS_SERVER_LOG")

    info "启动 pishoo 反向代理，监听端口：$PISHOO_PORT"
    GENMETA_HOME="$GENMETA_HOME_DIR" RUST_LOG="$RUST_LOG_VALUE" \
        "$PISHOO_BIN" -c "$PISHOO_CONF" >"$PISHOO_LOG" 2>&1 &
    PIDS+=("$!")
    PROCESS_NAMES+=("pishoo")
    PROCESS_LOGS+=("$PISHOO_LOG")

    info "启动 forward 正向代理，监听端口：$FORWARD_PORT"
    RUST_LOG="$RUST_LOG_VALUE" \
        "$FORWARD_BIN" "$FORWARD_CONF" >"$FORWARD_LOG" 2>&1 &
    PIDS+=("$!")
    PROCESS_NAMES+=("forward")
    PROCESS_LOGS+=("$FORWARD_LOG")

    info '三个关键进程均已拉起，开始进入健康检查阶段'
}

wait_for_direct_upstream() {
    local response

    # 先直接验证 upstream 本身是否就绪，避免把所有问题都混在代理链路里排查。
    # 这里使用单独的探测证书，便于和 pishoo 真正使用的客户端证书区分开。
    for _ in $(seq 1 30); do
        ensure_processes_running
        if response=$(curl --silent --show-error --fail \
            --connect-timeout 1 \
            --max-time 3 \
            --cacert "$CA_CERT" \
            --cert "$PROBE_CLIENT_CERT" \
            --key "$PROBE_CLIENT_KEY" \
            "https://localhost:$UPSTREAM_PORT/direct-check" 2>/dev/null); then
            if [[ "$response" == *"upstream-https-ok"* ]]; then
                info '本地 HTTPS upstream 已就绪，且可用客户端证书成功访问'
                return 0
            fi
        fi
        sleep 1
    done

    error '等待本地 HTTPS upstream 就绪超时'
    return 1
}

assert_upstream_requires_client_cert() {
    # 确认 upstream 确实要求客户端证书，证明 mTLS 验证链路在工作。
    ensure_processes_running

    if curl --silent --show-error --fail \
        --connect-timeout 1 \
        --max-time 3 \
        --cacert "$CA_CERT" \
        "https://localhost:$UPSTREAM_PORT/without-client-cert" >/dev/null 2>&1; then
        error 'HTTPS upstream 错误地接受了“未携带客户端证书”的请求'
        return 1
    fi

    info '已确认 HTTPS upstream 会强制校验客户端证书'
}

print_pishoo_client_certificate_info() {
    local cert_info

    cert_info=$(python3 - "$HTTPS_SERVER_LOG" <<'PY'
import sys

log_path = sys.argv[1]
target = None
with open(log_path, 'r', encoding='utf-8', errors='replace') as fh:
    for line in fh:
        if 'UPSTREAM_CLIENT_CERT' in line and 'subject_cn=proxy-client' in line:
            target = line.rstrip('\n')

if target:
    print(target)
PY
)

    if [[ -z "$cert_info" ]]; then
        error '未在 upstream 日志中找到 pishoo 使用的客户端证书信息'
        return 1
    fi

    mkdir -p "$(dirname -- "$UPSTREAM_CLIENT_CERT_INFO_FILE")"
    printf '%s\n' "$cert_info" > "$UPSTREAM_CLIENT_CERT_INFO_FILE"

    info 'upstream 观察到 pishoo 反代时使用的客户端证书信息：'
    printf '%s\n' "$cert_info"
    info "已将该证书信息写入单独文件：$UPSTREAM_CLIENT_CERT_INFO_FILE"
}

wait_for_proxy_chain() {
    local response

    # 最后验证完整链路：curl -> forward -> pishoo -> HTTPS upstream。
    for _ in $(seq 1 45); do
        ensure_processes_running
        if response=$(curl --silent --show-error --fail \
            --connect-timeout 1 \
            --max-time 5 \
            -x "http://127.0.0.1:$FORWARD_PORT" \
            "http://$TARGET_HOST/chain-check?via=forward" 2>/dev/null); then
            if [[ "$response" == *"upstream-https-ok"* && "$response" == *"path=/chain-check?via=forward"* ]]; then
                printf '[信息] 完整代理链路验证成功，upstream 返回内容如下：\n%s\n' "$response"
                return 0
            fi
        fi
        sleep 1
    done

    error '等待完整代理链路就绪超时'
    return 1
}

main() {
    # 核心流程：
    # 1. 编译所需二进制
    # 2. 生成一套临时证书
    # 3. 写出 pishoo / forward 配置
    # 4. 启动 upstream、pishoo、forward
    # 5. 先验证 upstream，再验证整条代理链路
    info "仓库目录：$REPO_ROOT"
    info "临时目录：$TMP_DIR"

    build_targets
    generate_certificates
    write_configs
    start_processes
    wait_for_direct_upstream
    assert_upstream_requires_client_cert
    wait_for_proxy_chain
    print_pishoo_client_certificate_info

    info '端到端 HTTPS upstream 代理链路验证通过'
}

main "$@"

#!/bin/sh
exec python3 - "$@" <<'PY'
import base64
import json
import subprocess
import sys

if len(sys.argv) < 2:
    print("usage: ./test.sh PROGRAM [ARG ...]", file=sys.stderr)
    raise SystemExit(2)

response = subprocess.run(
    [
        "curl", "--silent", "--show-error", "--fail-with-body", "--max-time", "40",
        "--header", "Content-Type: application/json", "--data-binary", "@-",
        "http://127.0.0.1:8472/exec",
    ],
    input=json.dumps({"program": sys.argv[1], "args": sys.argv[2:]}).encode(),
    capture_output=True,
)
if response.returncode:
    detail = response.stdout if response.returncode == 22 else response.stderr
    print(f"/exec: {detail.decode(errors='replace').strip()}", file=sys.stderr)
    raise SystemExit(1)

result = json.loads(response.stdout)
sys.stdout.buffer.write(base64.b64decode(result["stdout_base64"], validate=True))
sys.stdout.buffer.flush()
sys.stderr.buffer.write(base64.b64decode(result["stderr_base64"], validate=True))
sys.stderr.buffer.flush()
raise SystemExit(result["exit_code"] if result["exit_code"] is not None else 128 + result["signal"])
PY

#!/usr/bin/env python3
"""Run one interactive, full-duplex Echo request over QUIC or loopback TCP."""

import argparse
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[2]
CLIENT = ROOT / "target/debug/examples/pishoo-client"
SERVER = ROOT / "target/debug/pishoo"
SETUP = ROOT / "target/debug/examples/setup-tcp-demo"
DEMO_URL = "https://demo.dhttp.net/api/echo/echo"
DEMO_PORTS = "demo.dhttp.net=18472,upstream.dhttp.net=18473"


def build(transport: str, local_demo: bool) -> None:
    command = ["cargo", "build", "--locked", "-p", "pishoo", "--example", "pishoo-client"]
    if transport == "tcp":
        command += ["--features", "tcp-mock"]
    if local_demo:
        command += ["--bin", "pishoo", "--example", "setup-tcp-demo"]
    subprocess.run(command, cwd=ROOT, check=True)


def echo(env: dict[str, str], url: str, identity: str) -> None:
    env["PISHOO_CLIENT_IDENTITY"] = identity
    subprocess.run([CLIENT, "echo", url], cwd=ROOT, env=env, check=True)


def local_tcp_demo(env: dict[str, str]) -> None:
    with tempfile.TemporaryDirectory(prefix="pishoo-echo-") as home:
        env["DHTTP_HOME"] = home
        env.setdefault("DHTTP_TCP_MOCK_PORTS", DEMO_PORTS)
        subprocess.run([SETUP], cwd=ROOT, env=env, check=True)
        server = subprocess.Popen([SERVER], cwd=ROOT, env=env)
        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise RuntimeError(f"Pishoo exited with status {server.returncode}")
                try:
                    ready = subprocess.run(
                        [CLIENT, "get", "/hello.txt"],
                        cwd=ROOT,
                        env=env,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        timeout=2,
                    )
                    if ready.returncode == 0:
                        break
                except subprocess.TimeoutExpired:
                    pass
                time.sleep(0.1)
            else:
                raise RuntimeError("Pishoo TCP listener did not become ready")
            echo(env, DEMO_URL, "demo.dhttp.net")
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--transport", choices=["tcp", "quic"], default="tcp")
    parser.add_argument("--url", help="existing Echo URL; required for QUIC")
    parser.add_argument("--identity", help="local DHTTP identity used by the client")
    args = parser.parse_args()
    local_demo = args.transport == "tcp" and args.url is None
    if args.transport == "quic" and args.url is None:
        parser.error("QUIC requires --url and an existing DHTTP_HOME identity")
    env = os.environ.copy()
    if not local_demo and "DHTTP_HOME" not in env:
        parser.error("set DHTTP_HOME when connecting to an existing server")
    if not local_demo and args.transport == "tcp" and "DHTTP_TCP_MOCK_PORTS" not in env:
        parser.error("set DHTTP_TCP_MOCK_PORTS for an existing TCP server")
    build(args.transport, local_demo)
    if local_demo:
        local_tcp_demo(env)
    else:
        identity = args.identity or urlsplit(args.url).hostname
        if not identity:
            parser.error("provide --identity or a URL with an identity host")
        echo(env, args.url, identity)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(130)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Echo example: {error}", file=sys.stderr)
        raise SystemExit(1)

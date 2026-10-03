#!/usr/bin/env python3
"""Run one interactive, full-duplex Echo request over QUIC."""

import argparse
import os
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[2]
CLIENT = ROOT / "target/debug/examples/pishoo-client"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--transport", choices=["quic"], default="quic")
    parser.add_argument("--url", required=True, help="existing Echo URL")
    parser.add_argument("--identity", help="local DHTTP identity used by the client")
    args = parser.parse_args()
    env = os.environ.copy()
    if "DHTTP_HOME" not in env:
        parser.error("set DHTTP_HOME when connecting to an existing server")
    identity = args.identity or urlsplit(args.url).hostname
    if not identity:
        parser.error("provide --identity or a URL with an identity host")
    env["PISHOO_CLIENT_IDENTITY"] = identity
    subprocess.run(
        ["cargo", "build", "--locked", "-p", "pishoo", "--example", "pishoo-client"],
        cwd=ROOT,
        check=True,
    )
    subprocess.run([CLIENT, "echo", args.url], cwd=ROOT, env=env, check=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(130)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Echo example: {error}", file=sys.stderr)
        raise SystemExit(1)

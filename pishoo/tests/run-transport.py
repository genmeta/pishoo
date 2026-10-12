#!/usr/bin/env python3
"""Build and run isolated real QUIC acceptance; retain results and process samples."""
import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soak-seconds", type=int, default=1800)
    parser.add_argument("--case", choices=["matrix", "process"], default="matrix")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.soak_seconds < 0:
        parser.error("--soak-seconds must be nonnegative")
    repo = Path(__file__).resolve().parents[2]
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output or repo / "target" / ("transport-" + stamp)
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    env["PISHOO_TRANSPORT_RESULTS"] = str(output / "results.jsonl")
    env["PISHOO_TRANSPORT_SOAK_SECONDS"] = str(args.soak_seconds)
    if args.case == "process":
        env["PISHOO_TRANSPORT_PROCESS_EVIDENCE"] = str(output / "processes")
    if "DHTTP_TEST_OPENSSL" not in env and Path("/opt/homebrew/bin/openssl").exists():
        env["DHTTP_TEST_OPENSSL"] = "/opt/homebrew/bin/openssl"
    # Cargo discovers configuration from cwd. Avoid unrelated ancestor path patches.
    command = ["cargo", "test", "--locked", "--offline", "--manifest-path", str(repo / "Cargo.toml"), "-p", "pishoo", "--lib", "--no-run", "--message-format=json"]
    print("Evidence directory:", output, flush=True)
    with (output / "build.log").open("w") as log:
        build = subprocess.run(command, cwd=tempfile.gettempdir(), env=env, stdout=subprocess.PIPE, stderr=log, text=True)
    (output / "build.jsonl").write_text(build.stdout)
    if build.returncode:
        print("Build failed; see", output / "build.log", flush=True)
        return build.returncode
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith("{")]
    executable = next(item["executable"] for item in artifacts if item.get("reason") == "compiler-artifact" and item.get("executable") and item.get("profile", {}).get("test") and item["target"]["name"] == "pishoo")
    case = "server::network_tests::transport::" + (
        "transport_large_duplex_concurrency_acceptance" if args.case == "matrix"
        else "transport_cross_process_acceptance"
    )
    test_command = [executable, case, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
    metadata = {
        "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(),
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "build_command": command, "command": test_command,
        "soak_seconds": args.soak_seconds if args.case == "matrix" else 0,
        "case": args.case, "profile": "debug", "worker_threads": 4,
        "topology": "one process; three named identities; local UDP; production Router; local HTTP upstream" if args.case == "matrix" else "one router process and two QUIC client processes; local UDP",
        "resource_scope": "combined client/server test process" if args.case == "matrix" else "parent orchestrator only; child resources are not sampled",
    }
    started = time.monotonic()
    with (output / "test.log").open("w") as log, (output / "resources.jsonl").open("w") as samples:
        process = subprocess.Popen(test_command, cwd=repo, env=env, stdout=log, stderr=subprocess.STDOUT)
        metadata["pid"] = process.pid
        (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
        deadline = started + args.soak_seconds + 3600
        next_fds = 0
        try:
            while process.poll() is None:
                elapsed = time.monotonic() - started
                sample = {"elapsed_s": elapsed}
                try:
                    value = subprocess.check_output(["ps", "-o", "rss=,%cpu=", "-p", str(process.pid)], text=True, stderr=subprocess.DEVNULL).split()
                    if len(value) == 2:
                        sample.update(rss_kib=int(value[0]), cpu_percent=float(value[1]))
                    if elapsed >= next_fds:
                        listing = subprocess.run(["lsof", "-p", str(process.pid), "-Ff"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=5)
                        sample["fd_count"] = sum(line.startswith("f") and line[1:].isdigit() for line in listing.stdout.splitlines())
                        next_fds = elapsed + 10
                except (OSError, subprocess.SubprocessError):
                    pass
                samples.write(json.dumps(sample) + "\n")
                samples.flush()
                if time.monotonic() > deadline:
                    process.terminate()
                    metadata["harness_timeout"] = True
                    break
                time.sleep(1)
            try:
                code = process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                code = process.wait()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    metadata["exit_code"] = code
    metadata["elapsed_s"] = time.monotonic() - started
    metadata["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    results = [json.loads(line) for line in (output / "results.jsonl").read_text().splitlines()] if (output / "results.jsonl").exists() else []
    if args.case == "process":
        results = [json.loads(line) for path in sorted((output / "processes").glob("*.jsonl")) for line in path.read_text().splitlines()]
        (output / "results.jsonl").write_text("".join(json.dumps(row) + "\n" for row in results))
    print("Completed:", sum(row["status"] == "passed" for row in results), "passed;", sum(row["status"] == "failed" for row in results), "failed; exit", code, flush=True)
    for row in results:
        if row["status"] == "failed":
            print(row["case"], row["error"], flush=True)
    return code


if __name__ == "__main__":
    raise SystemExit(main())

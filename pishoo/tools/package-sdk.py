#!/usr/bin/env python3
"""Package the Rust SDK and standalone Lib examples without publishing them."""

import argparse
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path


def main() -> int:
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-dir", type=Path, default=repo / "target/sdk-dist",
        help="destination for the .crate and developer .tar.gz archives",
    )
    parser.add_argument("--offline", action="store_true", help="use cached Cargo dependencies")
    args = parser.parse_args()
    try:
        manifest = tomllib.loads((repo / "wit/Cargo.toml").read_text())
        name = manifest["package"]["name"]
        version = manifest["package"]["version"]
        archive_name = f"{name}-{version}"
        output = args.output_dir.resolve()
        output.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".package-sdk-", dir=output) as temporary:
            stage = Path(temporary)
            bundle = stage / archive_name
            sdk = bundle / "sdk"
            sdk.mkdir(parents=True)
            for filename in ["Cargo.toml", "Cargo.lock", "README.md"]:
                shutil.copy2(repo / "wit" / filename, sdk / filename)
            shutil.copytree(repo / "wit/src", sdk / "src")
            for interface in ["pishoo-identity", "pishoo-identity-v2"]:
                shutil.copytree(repo / "wit" / interface, sdk / interface)
            shutil.copy2(repo / "LICENSE", sdk / "LICENSE")

            # Cargo verifies that the crate builds using only its packaged files.
            command = [
                "cargo", "package", "--locked", "--allow-dirty",
                "--manifest-path", str(sdk / "Cargo.toml"),
                "--target-dir", str(stage / "target"),
            ]
            if args.offline:
                command.append("--offline")
            subprocess.run(command, check=True)

            for example in ["journal"]:
                source = repo / "pishoo/examples" / example
                destination = bundle / "examples" / example
                destination.mkdir(parents=True)
                for filename in ["Cargo.toml", "Cargo.lock", "README.md", "build.sh", "openapi.json", ".gitignore"]:
                    shutil.copy2(source / filename, destination / filename)
                shutil.copytree(source / "src", destination / "src")
                if example == "journal":
                    shutil.copytree(source / "public", destination / "public")
                    shutil.copy2(source / "wasmer.toml", destination / "wasmer.toml")
                    shutil.copy2(source / "app.toml", destination / "app.toml")
                cargo_manifest = destination / "Cargo.toml"
                text = cargo_manifest.read_text()
                original = 'path = "../../../wit"'
                if text.count(original) != 1:
                    raise ValueError(f"{example}: expected exactly one repository SDK dependency")
                cargo_manifest.write_text(text.replace(original, 'path = "../../sdk"'))
                if example == "journal":
                    readme = destination / "README.md"
                    readme.write_text(readme.read_text().replace(
                        "[SDK 文档](../../docs/sdk.md)", "[SDK 文档](../../README.md)",
                    ))

            tools = bundle / "tools"
            tools.mkdir()
            shutil.copy2(repo / "pishoo/tools/package-lib.py", tools / "package-lib.py")
            shutil.copy2(repo / "pishoo/docs/sdk.md", bundle / "README.md")
            shutil.copy2(repo / "pishoo/docs/host-capabilities.md", bundle / "host-capabilities.md")
            guide = repo / "pishoo/docs/developing-wasm.md"
            (bundle / "developing-wasm.md").write_text(
                guide.read_text().replace("[SDK 文档](sdk.md)", "[SDK 文档](README.md)"),
            )
            shutil.copy2(repo / "LICENSE", bundle / "LICENSE")
            crate_name = f"{archive_name}.crate"
            crate = stage / "target/package" / crate_name
            with tarfile.open(stage / f"{archive_name}.tar.gz", "w:gz") as archive:
                archive.add(bundle, arcname=archive_name)
            os.replace(crate, output / crate_name)
            os.replace(stage / f"{archive_name}.tar.gz", output / f"{archive_name}.tar.gz")
        print(f"Wrote {output / crate_name}")
        print(f"Wrote {output / f'{archive_name}.tar.gz'}")
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"package-sdk: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

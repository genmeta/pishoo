#!/usr/bin/env python3
"""Attach a Pishoo OpenAPI manifest to a WASM component."""

import argparse
import json
import os
import sys
import tempfile
from pathlib import Path

COMPONENT_HEADER = b"\x00asm\x0d\x00\x01\x00"
SECTION_NAME = b"pishoo:openapi"


def read_uleb(data: bytes, offset: int) -> tuple[int, int]:
    value = 0
    for shift in range(0, 35, 7):
        if offset >= len(data):
            raise ValueError("truncated WASM section length")
        byte = data[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        if byte < 0x80:
            return value, offset
    raise ValueError("WASM section length is too large")


def write_uleb(value: int) -> bytes:
    result = bytearray()
    while value >= 0x80:
        result.append((value & 0x7F) | 0x80)
        value >>= 7
    result.append(value)
    return bytes(result)


def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate OpenAPI key: {key}")
        result[key] = value
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("component", type=Path, help="input WASM component")
    parser.add_argument("openapi", type=Path, help="OpenAPI 3.1 JSON file")
    parser.add_argument("output", type=Path, help="output lib.wasm")
    args = parser.parse_args()
    try:
        component = args.component.read_bytes()
        if not component.startswith(COMPONENT_HEADER):
            raise ValueError("input must be a WASM component; run wasm-tools component new first")
        manifest = args.openapi.read_bytes()
        if len(manifest) > 1024 * 1024:
            raise ValueError("OpenAPI JSON exceeds 1 MiB")
        document = json.loads(manifest.decode("utf-8"), object_pairs_hook=unique_object)
        if not isinstance(document, dict) or not str(document.get("openapi", "")).startswith("3.1."):
            raise ValueError("OpenAPI must be a 3.1.x JSON object")
        if not isinstance(document.get("paths"), dict):
            raise ValueError("OpenAPI paths must be an object")
        offset = len(COMPONENT_HEADER)
        while offset < len(component):
            section_id = component[offset]
            size, payload_start = read_uleb(component, offset + 1)
            end = payload_start + size
            if end > len(component):
                raise ValueError("truncated WASM section")
            if section_id == 0:
                name_size, name_start = read_uleb(component, payload_start)
                if name_start + name_size > end:
                    raise ValueError("truncated WASM custom section name")
                if component[name_start:name_start + name_size] == SECTION_NAME:
                    raise ValueError("component already has a pishoo:openapi section")
            offset = end
        payload = write_uleb(len(SECTION_NAME)) + SECTION_NAME + manifest
        output = component + b"\x00" + write_uleb(len(payload)) + payload
        if len(output) > 64 * 1024 * 1024:
            raise ValueError("result exceeds Pishoo's 64 MiB component limit")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=args.output.parent, delete=False) as staged:
            staged.write(output)
            staged.flush()
            os.fsync(staged.fileno())
            stage_path = Path(staged.name)
        os.replace(stage_path, args.output)
        print(f"Wrote {args.output}")
        return 0
    except (OSError, ValueError) as error:
        print(f"package-lib: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

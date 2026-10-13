#!/usr/bin/env python3
"""Attach a Pishoo OpenAPI manifest to a WASM component."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
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
    parser.add_argument("--certificate", type=Path, help="publisher fullchain.crt (leaf first)")
    parser.add_argument("--private-key", type=Path, help="publisher privkey.pem; never copied into output")
    parser.add_argument("--openssl", default="openssl", help="OpenSSL 3 executable for publisher signing")
    args = parser.parse_args()
    try:
        component = args.component.read_bytes()
        if bool(args.certificate) != bool(args.private_key):
            raise ValueError("--certificate and --private-key must be provided together")
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
                if component[name_start:name_start + name_size] == b"pishoo:signature":
                    raise ValueError("input component must not already be signed")
            offset = end
        payload = write_uleb(len(SECTION_NAME)) + SECTION_NAME + manifest
        output = component + b"\x00" + write_uleb(len(payload)) + payload
        if args.certificate:
            extension = document.get("x-pishoo")
            publisher = extension.get("publisher") if isinstance(extension, dict) else None
            if not isinstance(publisher, str) or not publisher:
                raise ValueError("publisher is required for signing")
            chain = re.findall(
                rb"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----",
                args.certificate.read_bytes(), re.DOTALL,
            )
            if not chain:
                raise ValueError("certificate chain must contain PEM certificates, leaf first")
            def openssl(*arguments: str, data: bytes | None = None) -> bytes:
                return subprocess.run(
                    [args.openssl, *arguments], input=data, capture_output=True, check=True,
                ).stdout
            names = openssl("x509", "-noout", "-ext", "subjectAltName", data=chain[0]).decode()
            if publisher not in re.findall(r"DNS:([^,\s]+)", names):
                raise ValueError("publisher does not match signing certificate")
            certificates = [openssl("x509", "-outform", "DER", data=cert) for cert in chain]
            public = openssl("x509", "-pubkey", "-noout", data=chain[0])
            key = openssl("pkey", "-pubin", "-text", "-noout", data=public).decode()
            # Match dhttp-home's canonical signing schemes exactly.
            if "ED25519" in key:
                algorithm = ["pkeyutl", "-rawin"]
            elif "prime256v1" in key:
                algorithm = ["dgst", "-sha256"]
            elif "secp384r1" in key:
                algorithm = ["dgst", "-sha384"]
            elif "Modulus:" in key:
                algorithm = ["dgst", "-sha512", "-sigopt", "rsa_padding_mode:pss",
                             "-sigopt", "rsa_pss_saltlen:digest"]
            else:
                raise ValueError("unsupported signing key (use Ed25519, P-256, P-384 or RSA)")
            with tempfile.TemporaryDirectory(prefix="pishoo-sign-") as directory:
                stage = Path(directory)
                (stage / "input").write_bytes(b"pishoo-lib-signature-v1\0" + hashlib.sha256(output).digest())
                (stage / "public.pem").write_bytes(public)
                if algorithm[0] == "pkeyutl":
                    signature = openssl(*algorithm, "-sign", "-inkey", str(args.private_key), "-in", str(stage / "input"))
                else:
                    signature = openssl(*algorithm, "-sign", str(args.private_key), str(stage / "input"))
                if not signature or len(signature) > 8192:
                    raise ValueError("signature exceeds the store limit")
                (stage / "signature").write_bytes(signature)
                if algorithm[0] == "pkeyutl":
                    openssl(*algorithm, "-verify", "-pubin", "-inkey", str(stage / "public.pem"),
                            "-sigfile", str(stage / "signature"), "-in", str(stage / "input"))
                else:
                    openssl(*algorithm, "-verify", str(stage / "public.pem"),
                            "-signature", str(stage / "signature"), str(stage / "input"))
            envelope = json.dumps({"version": 1,
                "certificates": [base64.b64encode(cert).decode() for cert in certificates],
                "signature": base64.b64encode(signature).decode()}, separators=(",", ":")).encode()
            if len(envelope) > 64 * 1024:
                raise ValueError("signature envelope exceeds 64 KiB")
            name = b"pishoo:signature"
            payload = write_uleb(len(name)) + name + envelope
            output += b"\x00" + write_uleb(len(payload)) + payload
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
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"package-lib: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

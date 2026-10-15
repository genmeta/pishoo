#!/usr/bin/env python3
"""Build/pack std with the real std signer; test fresh users over real HTTP/3.

Requires WASI_SDK_PATH, OpenSSL 3, a new gmutils genmeta and local Rust dependencies.
Private credentials are copied only into an automatically removed temporary HOME.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--genmeta', type=Path, required=True)
    parser.add_argument('--cargo-config', type=Path)
    parser.add_argument('--bun', type=Path, help='Bun version compatible with the std lockfiles')
    parser.add_argument('--skip-build', action='store_true')
    parser.add_argument('--packages', nargs='+', help='Limit a diagnostic run; omit for every std package')
    args = parser.parse_args()
    environment = os.environ.copy()
    # LibreSSL's OCSP output differs; prefer the installed OpenSSL 3 when available.
    openssl = Path('/opt/homebrew/opt/openssl@3/bin/openssl')
    if openssl.exists():
        environment.setdefault('DHTTP_TEST_OPENSSL', str(openssl))
    sources = [(kind, source) for kind in ('libs', 'apps')
               for source in sorted((repo / 'std' / kind).iterdir())
               if (source / 'wasmer.toml').is_file()
               and (not args.packages or source.name in args.packages)]
    with tempfile.TemporaryDirectory(prefix='pishoo-std-workflow-') as temporary:
        stage = Path(temporary)
        tools = stage / 'bin'
        tools.mkdir()
        if args.bun:
            (tools / 'bun').symlink_to(args.bun.resolve())
        for name, path in [('genmeta', args.genmeta.resolve()),
                           ('pishoo', repo / 'target/debug/pishoo')]:
            if name == 'genmeta':
                shutil.copy2(path, tools / name)
            else:
                (tools / name).symlink_to(path)
        # Keep the actual launcher next to the selected packer, regardless of its source directory.
        environment['PATH'] = str(tools) + os.pathsep + environment['PATH']
        cargo = ['cargo', '--offline']
        if args.cargo_config:
            cargo += ['--config', str(args.cargo_config.resolve())]
        cwd = tempfile.gettempdir()
        if not args.skip_build:
            subprocess.run([cargo[0], 'build'] + cargo[1:] + ['--manifest-path', str(repo / 'Cargo.toml'),
                           '-p', 'pishoo', '--bins'], cwd=cwd, env=environment, check=True)
        projects = stage / 'projects'
        projects.mkdir()
        for kind, source in sources:
            if not args.skip_build:
                if (source / 'build.sh').is_file():
                    subprocess.run(['sh', str(source / 'build.sh'), '--publisher', 'std.dhttp.net'],
                                   cwd=repo, env=environment, check=True)
                elif kind == 'apps':
                    subprocess.run(['bun', 'run', 'build'], cwd=source, env=environment, check=True)
                else:
                    raise SystemExit('No build script for ' + str(source))
            project = projects / (kind + '-' + source.name)
            project.mkdir()
            manifest = (source / 'wasmer.toml').read_text().replace('dev.dhttp.net', 'std.dhttp.net')
            if kind == 'apps':
                shutil.copytree(source / 'dist', project / 'public')
                manifest = manifest.replace('"/public" = "."', '"/public" = "public"')
            else:
                shutil.copyfile(source / 'dist/lib.wasm', project / 'lib.wasm')
                shutil.copytree(source / 'dist/metadata', project / 'metadata')
            (project / 'wasmer.toml').write_text(manifest)
        environment['GENMETA_PACK_TEST_BIN'] = str(tools / 'genmeta')
        environment['PISHOO_STD_E2E_PROJECTS'] = str(projects)
        command = [cargo[0], 'test'] + cargo[1:]
        command += ['--manifest-path', str(repo / 'Cargo.toml'), '-p', 'pishoo', '--lib',
                    'clean_identities_pack_install_friendship_approval_order_and_chat',
                    '--', '--ignored', '--nocapture', '--test-threads=1']
        print('Testing all selected std sources:', json.dumps([str(source.relative_to(repo)) for _, source in sources]), flush=True)
        subprocess.run(command, cwd=cwd, env=environment, check=True)


if __name__ == '__main__':
    main()

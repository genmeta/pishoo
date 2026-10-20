#!/usr/bin/env python3
"""Rebuild local Workspace packages and their development data, then run Pishoo."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time


PROJECT = Path(__file__).resolve().parent
REPO = PROJECT.parents[2]
PACKAGES = (
    ('message', 'lib', 'message-lib.webc'),
    ('profile', 'lib', 'profile-lib.webc'),
    ('workspace', 'app', 'workspace-app.webc'),
)
API_ROOTS = tuple('/std/' + name for name, _kind, _filename in PACKAGES)


def identity_name(value):
    value = value.strip().lower()
    if value.endswith('~'):
        value = value[:-1] + '.dhttp.net'
    elif not value.endswith('.dhttp.net'):
        value += '.dhttp.net'
    if len(value) > 253 or not all(
        len(label) <= 63 and re.fullmatch(r'[a-z0-9](?:[a-z0-9-]*[a-z0-9])?', label)
        for label in value.split('.')
    ):
        raise argparse.ArgumentTypeError('请输入有效的身份名，例如 alice.smith~')
    return value


def announce(message):
    print(f'[workspace] {message}', flush=True)


def require_tool(name):
    executable = shutil.which(name)
    if executable is None:
        raise RuntimeError(f'找不到 {name}，请先安装或设置 PATH。')
    return str(Path(executable).absolute())


def ensure_host_stopped():
    result = subprocess.run(
        ['pgrep', '-x', 'pishoo'], capture_output=True, text=True, check=False,
    )
    if result.returncode == 0:
        raise RuntimeError(
            f'已有 Pishoo 进程运行（PID {", ".join(result.stdout.split())}）。'
            '请先停止旧服务，再运行此脚本，以确保访问的是本分支宿主。'
        )
    if result.returncode != 1:
        raise RuntimeError('无法检查已有 Pishoo 进程，请检查 pgrep。')


def prerequisites(args):
    for tool in ('cargo', 'bun', 'wasm-tools', 'pgrep'):
        require_tool(tool)
    genmeta = require_tool(args.genmeta)
    help_result = subprocess.run(
        [genmeta, 'package', '--help'], capture_output=True, text=True,
        timeout=10, check=False,
    )
    if help_result.returncode != 0:
        raise RuntimeError(
            f'{genmeta} 不支持 genmeta package。请使用提供 package build/install/list 的新版 gmutils；'
            '可通过 --genmeta /path/to/genmeta 指定。'
        )
    sdk_value = os.environ.get('WASI_SDK_PATH')
    if not sdk_value or not (Path(sdk_value).expanduser() / 'bin/clang').is_file():
        raise RuntimeError('请设置 WASI_SDK_PATH，指向包含 bin/clang 的 WASI SDK 目录。')
    ensure_host_stopped()
    environment = os.environ.copy()
    environment['WASI_SDK_PATH'] = str(Path(sdk_value).expanduser().resolve())
    environment['GENMETA'] = genmeta
    environment.setdefault('RUST_LOG', 'info')
    return environment


def run(command, environment):
    subprocess.run(command, cwd=REPO, env=environment, check=True)


def wait_ready(host, genmeta, identity, environment, log_path):
    deadline = time.monotonic() + 90
    last_error = ''
    while time.monotonic() < deadline:
        if host.poll() is not None:
            raise RuntimeError(f'Pishoo 提前退出，请查看 {log_path}')
        try:
            result = subprocess.run(
                [genmeta, 'package', 'list', '--id', identity], cwd=REPO,
                env=environment, capture_output=True, text=True, timeout=5, check=False,
            )
            if result.returncode == 0:
                return
            last_error = result.stderr.strip() or result.stdout.strip()
        except subprocess.TimeoutExpired:
            last_error = '身份管理入口连接超时'
        time.sleep(1)
    raise RuntimeError(
        f'{identity} 的管理入口在 90 秒内未就绪：{last_error}\n'
        f'请确认身份凭据有效且已通过 genmeta identity ensite --id {identity} 启用，'
        f'并查看 {log_path}'
    )


def stop_host(host):
    if host.poll() is not None:
        return
    host.terminate()
    try:
        host.wait(timeout=10)
    except subprocess.TimeoutExpired:
        host.kill()
        host.wait()


def packages_to_remove(genmeta, identity, publisher, environment):
    result = subprocess.check_output(
        [genmeta, 'package', 'list', '--id', identity],
        cwd=REPO, env=environment, text=True, timeout=30,
    )
    catalog = json.loads(result)
    if not isinstance(catalog, list) or any(not isinstance(item, dict) for item in catalog):
        raise RuntimeError('软件包目录格式无效，无法确定本次重建范围。')
    targets = {(kind, f'{publisher}/{name}') for name, kind, _filename in PACKAGES}
    names = {(kind, name) for name, kind, _filename in PACKAGES}
    installed = set()
    for item in catalog:
        key = (item.get('kind'), item.get('id'))
        if (item.get('kind'), item.get('name')) in names and key not in targets:
            raise RuntimeError(f'同名软件包 {item.get("id")} 属于其他发布者，请沿用其 --publisher。')
        if key in targets:
            installed.add(key)
        else:
            for dependency in item.get('dependencies', []):
                dependency_id = f'{dependency["publisher"]}/{dependency["name"]}'
                if ('lib', dependency_id) in targets:
                    raise RuntimeError(
                        f'{item.get("id")} 依赖 {dependency_id}，无法仅重建 Workspace。'
                        '请先处理该依赖。'
                    )
    return [
        (kind, f'{publisher}/{name}')
        for name, kind, _filename in reversed(PACKAGES)
        if (kind, f'{publisher}/{name}') in installed
    ]


def clear_package_data(profile, publisher):
    paths = [profile / 'db' / publisher / name for name, kind, _filename in PACKAGES if kind == 'lib']
    access_db = profile / 'db/access.db'
    # Validate every deletion target before modifying either files or ACL.
    for path in [profile.parent, profile, profile / 'db', profile / 'db' / publisher, *paths, access_db]:
        if path.is_symlink():
            raise RuntimeError(f'开发数据路径包含符号链接，无法清理：{path}')
    for path in paths:
        if path.exists() and not path.is_dir():
            raise RuntimeError(f'开发数据路径不是目录：{path}')
    if access_db.exists() and not access_db.is_file():
        raise RuntimeError(f'ACL 数据库不是普通文件：{access_db}')
    if access_db.exists():
        with sqlite3.connect(access_db.as_uri() + '?mode=rw', uri=True) as database:
            for root in API_ROOTS:
                for table in ('access_rules', 'access_reviews'):
                    database.execute(
                        f'DELETE FROM {table} WHERE api = ? OR substr(api, 1, ?) = ?',
                        (root, len(root) + 1, root + '/'),
                    )
    for path in paths:
        if path.exists():
            shutil.rmtree(path)


def start(args):
    environment = prerequisites(args)
    publisher = args.publisher or args.identity
    announce('构建当前分支的 Pishoo（增量构建）…')
    run(['cargo', 'build', '--locked', '-p', 'pishoo', '--bin', 'pishoo'], environment)
    metadata = subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'],
        cwd=REPO, env=environment, text=True,
    )
    target = Path(json.loads(metadata)['target_directory'])
    host_path = target / 'debug/pishoo'
    if not host_path.is_file():
        raise RuntimeError(f'找不到本机构建的宿主 {host_path}，请检查 Cargo 的默认 target 配置。')
    output = target / 'workspace-dev'
    output.mkdir(parents=True, exist_ok=True)
    session = Path(tempfile.mkdtemp(prefix='run-', dir=output))
    log_path = session / 'pishoo.log'
    environment['PISHOO_WORKSPACE_DIST'] = str(session / 'packages')
    ensure_host_stopped()
    announce(f'启动 Pishoo；日志：{log_path}')
    with log_path.open('w') as log:
        host = subprocess.Popen(
            [str(host_path)], cwd=REPO, env=environment,
            stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            genmeta = environment['GENMETA']
            announce(f'等待 {args.identity} 的管理入口…')
            wait_ready(host, genmeta, args.identity, environment, log_path)
            if publisher != args.identity:
                announce(f'等待发布者 {publisher} 的签名校验入口…')
                wait_ready(host, genmeta, publisher, environment, log_path)
            announce(f'构建并签名 Message、Profile、Workspace；发布者：{publisher}')
            run(['sh', str(PROJECT / 'package.sh'), '--publisher', publisher], environment)
            removals = packages_to_remove(genmeta, args.identity, publisher, environment)
            announce('重建开发数据：卸载 Workspace、Profile、Message，并清除本项目业务数据与 ACL…')
            for kind, package_id in removals:
                run([
                    genmeta, 'package', 'remove', package_id,
                    '--type', kind, '--id', args.identity,
                ], environment)
            stop_host(host)
            ensure_host_stopped()
            profile = Path.home() / '.dhttp' / args.identity.removesuffix('.dhttp.net')
            clear_package_data(profile, publisher)
            announce('开发数据已清理，重新启动 Pishoo…')
            host = subprocess.Popen(
                [str(host_path)], cwd=REPO, env=environment,
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
            )
            wait_ready(host, genmeta, args.identity, environment, log_path)
            for name, kind, filename in PACKAGES:
                if host.poll() is not None:
                    raise RuntimeError(f'Pishoo 已退出，请查看 {log_path}')
                announce(f'安装 {publisher}/{name} 到 {args.identity}…')
                run([
                    genmeta, 'package', 'install', f'{publisher}/{name}',
                    '--file', str(session / 'packages' / filename),
                    '--type', kind, '--id', args.identity,
                ], environment)
            announce(f'Workspace 已就绪：https://{args.identity}/std/workspace/')
            announce(f'请在 AnySee 等支持 DHTTP 的浏览器中，以 {args.identity} 身份访问。')
            announce('保持此终端运行；Ctrl+C 停止本次 Pishoo。下次运行此脚本会再次清除本项目开发数据。')
            return host.wait()
        finally:
            stop_host(host)


def interrupted(_signum, _frame):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--id', dest='identity', type=identity_name, required=True,
                        help='本机已配置、已启用的目标身份；支持 alice.smith~')
    parser.add_argument('--publisher', type=identity_name,
                        help='签名发布者，默认与 --id 相同；需有本机凭据并已启用')
    parser.add_argument('--genmeta', default=os.environ.get('GENMETA', 'genmeta'),
                        help='支持 package 命令的 genmeta 路径，默认使用 GENMETA 或 PATH')
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, interrupted)
    try:
        return start(args)
    except KeyboardInterrupt:
        announce('已结束本次启动。')
        return 130
    except (OSError, RuntimeError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
        print(f'[workspace] 启动失败：{error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())

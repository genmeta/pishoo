#!/usr/bin/env python3
"""Check the development launcher with fake tools; no builds or identity writes."""

import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest


REPO = Path(__file__).resolve().parents[2]
FAKE_TOOL = r'''
import json
import os
from pathlib import Path
import sys

state = Path(os.environ['WORKSPACE_DEV_TEST_STATE'])
tool = Path(sys.argv[0]).name
args = sys.argv[1:]
if tool == 'tools.py':
    tool, args = args[0], args[1:]
with (state / 'calls.jsonl').open('a') as log:
    log.write(json.dumps([tool, *args]) + '\n')
if tool == 'pgrep':
    if os.environ.get('WORKSPACE_DEV_TEST_EXISTING_HOST'):
        print('12345')
        sys.exit(0)
    sys.exit(1)
if tool == 'cargo':
    target = state / 'target'
    if args[0] == 'metadata':
        print(json.dumps({'target_directory': str(target)}))
    elif args[0] == 'build':
        host = target / 'debug/pishoo'
        host.parent.mkdir(parents=True, exist_ok=True)
        host.write_text('#!' + sys.executable + '\n' + r"""
import os
from pathlib import Path
import signal
import time
state = Path(os.environ['WORKSPACE_DEV_TEST_STATE'])
def stop(_signal, _frame):
    (state / 'stopped').write_text('stopped')
    with (state / 'stopped-pids').open('a') as log:
        log.write(str(os.getpid()) + '\n')
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
(state / 'host.pid').write_text(str(os.getpid()))
with (state / 'host-pids').open('a') as log:
    log.write(str(os.getpid()) + '\n')
while True:
    time.sleep(0.05)
""")
        host.chmod(0o755)
    sys.exit(0)
if tool == 'package-script':
    if os.environ.get('WORKSPACE_DEV_TEST_FAIL_BUILD'):
        sys.exit(8)
    output = Path(os.environ['PISHOO_WORKSPACE_DIST'])
    output.mkdir(parents=True)
    for name in ('message-lib', 'profile-lib', 'workspace-app'):
        (output / (name + '.webc')).write_bytes(b'fake package')
    sys.exit(0)
if tool == 'genmeta':
    if args == ['package', '--help']:
        sys.exit(2 if os.environ.get('WORKSPACE_DEV_TEST_OLD_GENMETA') else 0)
    if args[:2] == ['package', 'list']:
        try:
            os.kill(int((state / 'host.pid').read_text()), 0)
        except (FileNotFoundError, ProcessLookupError):
            sys.exit(1)
        catalog = state / 'catalog.json'
        print(catalog.read_text() if catalog.exists() else '[]')
        sys.exit(0)
    if args[:2] == ['package', 'remove']:
        catalog = state / 'catalog.json'
        packages = json.loads(catalog.read_text())
        kind = args[args.index('--type') + 1]
        packages = [item for item in packages if (item['kind'], item['id']) != (kind, args[2])]
        catalog.write_text(json.dumps(packages))
        sys.exit(0)
    if args[:2] == ['package', 'install']:
        package = Path(args[args.index('--file') + 1])
        assert package.is_file(), str(package)
        if args[2].endswith('/' + os.environ.get('WORKSPACE_DEV_TEST_FAIL_INSTALL', '!')):
            sys.exit(7)
        catalog = state / 'catalog.json'
        packages = json.loads(catalog.read_text()) if catalog.exists() else []
        kind = args[args.index('--type') + 1]
        publisher, name = args[2].split('/')
        packages.append({'id': args[2], 'kind': kind, 'name': name, 'dependencies': []})
        catalog.write_text(json.dumps(packages))
        data = state / 'account/.dhttp/alice/db' / publisher / name
        data.mkdir(parents=True, exist_ok=True)
        (data / 'fresh').touch()
        sys.exit(0)
    raise SystemExit('Unexpected genmeta command: ' + repr(args))
raise SystemExit('Unexpected tool: ' + tool)
'''


class WorkspaceDevelopment(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='workspace dev ')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        project = self.root / 'std/apps/workspace'
        project.mkdir(parents=True)
        self.launcher = project / 'dev.py'
        shutil.copyfile(REPO / 'std/apps/workspace/dev.py', self.launcher)
        tools = self.root / 'bin'
        tools.mkdir()
        program = tools / 'tools.py'
        program.write_text('#!' + sys.executable + '\n' + FAKE_TOOL)
        program.chmod(0o755)
        for tool in ('cargo', 'bun', 'wasm-tools', 'pgrep', 'genmeta'):
            (tools / tool).symlink_to(program)
        (project / 'package.sh').write_text(
            '#!/bin/sh\nexec "$WORKSPACE_DEV_TEST_PYTHON" '
            '"$WORKSPACE_DEV_TEST_TOOLS" package-script "$@"\n'
        )
        sdk = self.root / 'sdk/bin'
        sdk.mkdir(parents=True)
        (sdk / 'clang').touch()
        self.environment = {
            **os.environ,
            'PATH': str(tools) + os.pathsep + os.environ.get('PATH', ''),
            'GENMETA': str(tools / 'genmeta'),
            'WASI_SDK_PATH': str(sdk.parent),
            'WORKSPACE_DEV_TEST_STATE': str(self.root),
            'WORKSPACE_DEV_TEST_PYTHON': sys.executable,
            'WORKSPACE_DEV_TEST_TOOLS': str(program),
        }

    def command(self, *args):
        runner = '''
from pathlib import Path
import runpy
import sys
from unittest.mock import patch
account = Path(sys.argv.pop(1))
sys.argv = sys.argv[1:]
with patch('pathlib.Path.home', return_value=account):
    runpy.run_path(sys.argv[0], run_name='__main__')
'''
        return [sys.executable, '-c', runner, str(self.root / 'account'),
                str(self.launcher), '--id', 'Alice~', *args]

    def calls(self):
        path = self.root / 'calls.jsonl'
        return [json.loads(line) for line in path.read_text().splitlines()]

    def assert_host_stopped(self):
        self.assertTrue((self.root / 'stopped').is_file())
        pids = (self.root / 'host-pids').read_text().splitlines()
        self.assertEqual((self.root / 'stopped-pids').read_text().splitlines(), pids)
        for pid in pids:
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid), 0)

    def test_builds_starts_and_installs_in_dependency_order_then_stops_its_host(self):
        log_path = self.root / 'launcher.log'
        self.run_until_ready('--publisher', 'signer~')
        calls = self.calls()
        build = next(call for call in calls if call[:2] == ['cargo', 'build'])
        self.assertEqual(build, ['cargo', 'build', '--locked', '-p', 'pishoo', '--bin', 'pishoo'])
        pack = next(call for call in calls if call[0] == 'package-script')
        self.assertEqual(pack, ['package-script', '--publisher', 'signer.dhttp.net'])
        installs = [call for call in calls if call[:3] == ['genmeta', 'package', 'install']]
        self.assertEqual([call[3] for call in installs], [
            'signer.dhttp.net/message', 'signer.dhttp.net/profile', 'signer.dhttp.net/workspace',
        ])
        self.assertEqual([call[call.index('--type') + 1] for call in installs], ['lib', 'lib', 'app'])
        self.assertTrue(all(call[-2:] == ['--id', 'alice.dhttp.net'] for call in installs))
        self.assertIn('https://alice.dhttp.net/std/workspace/', log_path.read_text())
        self.assert_host_stopped()

    def test_install_failure_stops_host_and_does_not_continue_installing(self):
        result = subprocess.run(
            self.command(), env={**self.environment, 'WORKSPACE_DEV_TEST_FAIL_INSTALL': 'profile'},
            capture_output=True, text=True, timeout=10, check=False,
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        installs = [call for call in self.calls() if call[:3] == ['genmeta', 'package', 'install']]
        self.assertEqual([call[3] for call in installs], [
            'alice.dhttp.net/message', 'alice.dhttp.net/profile',
        ])
        self.assertNotIn('Workspace 已就绪', result.stdout)
        self.assert_host_stopped()

    def test_existing_host_is_not_reused_or_stopped(self):
        result = subprocess.run(
            self.command(), env={**self.environment, 'WORKSPACE_DEV_TEST_EXISTING_HOST': '1'},
            capture_output=True, text=True, timeout=5, check=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn('已有 Pishoo 进程运行', result.stderr)
        self.assertFalse(any(call[0] == 'cargo' for call in self.calls()))
        self.assertFalse((self.root / 'host.pid').exists())

    def test_old_genmeta_fails_before_build_or_start(self):
        result = subprocess.run(
            self.command(), env={**self.environment, 'WORKSPACE_DEV_TEST_OLD_GENMETA': '1'},
            capture_output=True, text=True, timeout=5, check=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn('不支持 genmeta package', result.stderr)
        self.assertEqual(self.calls(), [['genmeta', 'package', '--help']])
        self.assertFalse((self.root / 'host.pid').exists())

    def run_until_ready(self, *args):
        log_path = self.root / 'launcher.log'
        with log_path.open('w') as log:
            process = subprocess.Popen(self.command(*args), env=self.environment,
                                       stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 15
                while 'Workspace 已就绪' not in log_path.read_text():
                    self.assertIsNone(process.poll(), log_path.read_text())
                    if time.monotonic() >= deadline:
                        self.fail(log_path.read_text())
                    time.sleep(0.05)
            finally:
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=5)
        self.assertEqual(process.returncode, 130, log_path.read_text())

    def test_repeated_launch_removes_dependents_and_resets_only_project_data(self):
        self.run_until_ready()
        profile = self.root / 'account/.dhttp/alice'
        project_data = profile / 'db/alice.dhttp.net/message'
        (project_data / 'old-message').touch()
        unrelated = profile / 'db/alice.dhttp.net/other'
        unrelated.mkdir()
        (unrelated / 'keep').touch()
        other_lib = profile / 'db/alice.dhttp.net/workspace'
        (other_lib / 'keep-lib-data').touch()
        credentials = profile / 'ssl/key.pem'
        credentials.parent.mkdir()
        credentials.write_text('fake identity key')
        access_db = profile / 'db/access.db'
        with sqlite3.connect(access_db) as db:
            db.execute('CREATE TABLE access_rules (api TEXT)')
            db.execute('CREATE TABLE access_reviews (api TEXT)')
            db.execute('CREATE TABLE contacts (name TEXT)')
            db.execute("INSERT INTO contacts VALUES ('friend')")
            for table in ('access_rules', 'access_reviews'):
                db.executemany(f'INSERT INTO {table} VALUES (?)', [
                    ('/std/message',), ('/std/profile/avatar',), ('/std/workspace/assets/a.js',),
                    ('/std/other',), ('/std/message-other',), ('/std/contact/status',),
                ])
        self.run_until_ready()
        removals = [call for call in self.calls() if call[:3] == ['genmeta', 'package', 'remove']]
        self.assertEqual([call[3] for call in removals], [
            'alice.dhttp.net/workspace', 'alice.dhttp.net/profile', 'alice.dhttp.net/message',
        ])
        self.assertFalse((project_data / 'old-message').exists())
        self.assertTrue((project_data / 'fresh').exists())
        self.assertTrue((unrelated / 'keep').is_file())
        self.assertTrue((other_lib / 'keep-lib-data').is_file())
        self.assertEqual(credentials.read_text(), 'fake identity key')
        with sqlite3.connect(access_db) as db:
            for table in ('access_rules', 'access_reviews'):
                self.assertEqual(db.execute(f'SELECT api FROM {table} ORDER BY api').fetchall(), [
                    ('/std/contact/status',), ('/std/message-other',), ('/std/other',),
                ])
            self.assertEqual(db.execute('SELECT name FROM contacts').fetchall(), [('friend',)])
        self.assert_host_stopped()

    def test_other_publisher_or_external_dependent_prevents_any_removal(self):
        for package in [
            {'kind': 'lib', 'name': 'message', 'id': 'someone.dhttp.net/message', 'dependencies': []},
            {'kind': 'app', 'name': 'other', 'id': 'someone.dhttp.net/other',
             'dependencies': [{'publisher': 'alice.dhttp.net', 'name': 'message'}]},
        ]:
            with self.subTest(package=package['id']):
                (self.root / 'catalog.json').write_text(json.dumps([package]))
                result = subprocess.run(self.command(), env=self.environment, capture_output=True,
                                        text=True, timeout=10, check=False)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertFalse(any(call[:3] == ['genmeta', 'package', 'remove'] for call in self.calls()))
                self.assertFalse(any(call[:3] == ['genmeta', 'package', 'install'] for call in self.calls()))
                self.assert_host_stopped()

    def test_data_symlink_is_rejected_without_deleting_its_destination(self):
        data = self.root / 'account/.dhttp/alice/db/alice.dhttp.net'
        data.mkdir(parents=True)
        elsewhere = self.root / 'unrelated'
        elsewhere.mkdir()
        (elsewhere / 'keep').touch()
        (data / 'message').symlink_to(elsewhere, target_is_directory=True)
        result = subprocess.run(self.command(), env=self.environment, capture_output=True,
                                text=True, timeout=10, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn('符号链接', result.stderr)
        self.assertTrue((elsewhere / 'keep').is_file())
        self.assertFalse(any(call[:3] == ['genmeta', 'package', 'install'] for call in self.calls()))
        self.assert_host_stopped()

    def test_build_failure_preserves_installed_packages_and_data(self):
        package = {'kind': 'lib', 'name': 'message', 'id': 'alice.dhttp.net/message', 'dependencies': []}
        catalog = self.root / 'catalog.json'
        catalog.write_text(json.dumps([package]))
        data = self.root / 'account/.dhttp/alice/db/alice.dhttp.net/message'
        data.mkdir(parents=True)
        (data / 'keep').touch()
        result = subprocess.run(
            self.command(), env={**self.environment, 'WORKSPACE_DEV_TEST_FAIL_BUILD': '1'},
            capture_output=True, text=True, timeout=10, check=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(catalog.read_text()), [package])
        self.assertTrue((data / 'keep').is_file())
        self.assertFalse(any(call[:3] == ['genmeta', 'package', 'remove'] for call in self.calls()))
        self.assert_host_stopped()


if __name__ == '__main__':
    unittest.main()

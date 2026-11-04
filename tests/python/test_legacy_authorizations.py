import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('legacy',ROOT/'tools/prepare-legacy-transfers.py')
legacy=importlib.util.module_from_spec(spec);spec.loader.exec_module(legacy)
FIXTURE=ROOT/'tests/fixtures/legacy-workspace'
OPENSSL='/opt/homebrew/bin/openssl' if Path('/opt/homebrew/bin/openssl').exists() else shutil.which('openssl')


class HistoricalAuthorizationTests(unittest.TestCase):
    def test_real_old_release_is_audited_without_fabricating_signature_history(self):
        target={'author':'dev.genmeta.tech.dhttp.net','owner_hash':'a'*64,'evidence':'TEST ONLY target owner'}
        entry=legacy.prepare(FIXTURE/'metadata.json',FIXTURE/'workspace-0.3.5.waz',target,FIXTURE/'root.crt',OPENSSL)
        self.assertEqual(entry['author'],'std.dhttp.net')
        self.assertEqual(entry['version'],'0.3.5')
        self.assertEqual(entry['target_author'],'dev.genmeta.tech.dhttp.net')
        self.assertIn('app/workspace/index.html',entry['files'])
        self.assertNotIn('signature',entry)
        with tempfile.TemporaryDirectory() as directory:
            metadata=json.loads((FIXTURE/'metadata.json').read_text())
            metadata['package_sha256']='f'*64
            path=Path(directory)/'metadata.json';path.write_text(json.dumps(metadata))
            with self.assertRaisesRegex(ValueError,'digest'):
                legacy.prepare(path,FIXTURE/'workspace-0.3.5.waz',target,FIXTURE/'root.crt',OPENSSL)

    def test_unverified_target_owner_is_rejected(self):
        with self.assertRaisesRegex(ValueError,'owner'):
            legacy.prepare(FIXTURE/'metadata.json',FIXTURE/'workspace-0.3.5.waz',{'author':'dev.genmeta.tech.dhttp.net','owner_hash':'','evidence':'TEST'},FIXTURE/'root.crt',OPENSSL)

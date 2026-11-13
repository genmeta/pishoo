import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from original_file_names import apply_volume, plan_volume

class OriginalNames(unittest.TestCase):
    def test_native_and_legacy_names_migrate_without_overwriting_or_changing_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            base=Path(temp);volume=base/'message';archive=volume/'.file/bob.dhttp.net/2026-11-06';archive.mkdir(parents=True)
            legacy='a'*64;resource='b'*32
            (archive/legacy).write_bytes(b'old')
            (archive/resource).mkdir();(archive/resource/'content').write_bytes(b'new')
            with sqlite3.connect(volume/'database') as db:
                db.execute('CREATE TABLE chat_messages(id INTEGER,contact_name TEXT,media_type TEXT,text TEXT)')
                value={'version':1,'name':'报告.txt','day':'2026-11-06','blake3':legacy,'size':3}
                db.execute('INSERT INTO chat_messages VALUES(1,?,?,?)',('bob.dhttp.net','application/vnd.pishoo.file+json',json.dumps(value)))
            with sqlite3.connect(volume/'.file/database') as db:
                db.execute('CREATE TABLE files(id TEXT,directory TEXT,name TEXT,size INTEGER,offset INTEGER,created_at INTEGER)')
                db.execute('INSERT INTO files VALUES(?,?,?,?,?,?)',(resource,'bob.dhttp.net/2026-11-06','报告.txt',3,3,1))
            plan=plan_volume(volume);self.assertEqual([p['target'].split('/')[-1] for p in plan],['报告.txt','报告 (2).txt'])
            apply_volume(volume,base/'backup')
            self.assertEqual((archive/'报告.txt').read_bytes(),b'old')
            self.assertEqual((archive/'报告 (2).txt').read_bytes(),b'new')
            self.assertFalse((archive/legacy).exists());self.assertFalse((archive/resource).exists())
            with sqlite3.connect(volume/'.file/database') as db:self.assertEqual(db.execute('SELECT id,storage_name FROM files').fetchone(),(resource,'报告 (2).txt'))
            self.assertEqual((base/'backup/.file/bob.dhttp.net/2026-11-06'/legacy).read_bytes(),b'old')
            self.assertEqual(plan_volume(volume),[])

    def test_bad_index_path_is_rejected_before_backup_or_changes(self):
        with tempfile.TemporaryDirectory() as temp:
            base=Path(temp);volume=base/'message';(volume/'.file').mkdir(parents=True)
            secret=base/'secret';secret.write_bytes(b'private')
            with sqlite3.connect(volume/'.file/database') as db:
                db.execute('CREATE TABLE files(id TEXT,directory TEXT,name TEXT,size INTEGER,offset INTEGER,created_at INTEGER)')
                db.execute('INSERT INTO files VALUES(?,?,?,?,?,?)',('a'*32,'../outside','secret',7,7,1))
            with self.assertRaises(ValueError):apply_volume(volume,base/'backup')
            self.assertFalse((base/'backup').exists());self.assertEqual(secret.read_bytes(),b'private')

if __name__=='__main__':unittest.main()

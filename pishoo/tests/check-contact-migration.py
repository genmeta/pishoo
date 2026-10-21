#!/usr/bin/env python3
"""Validate structural contact migrations with SQLite only; no Cargo build is needed."""
import os
import sqlite3
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DEPENDENCIES = tomllib.loads((ROOT / 'pishoo/Cargo.toml').read_text())['dependencies']


def daccess_migrations():
    override = os.environ.get('DACCESS_PATH')
    if override:
        return Path(override).expanduser().resolve() / 'src/migrations'
    dependency = DEPENDENCIES['access_control']
    if 'path' in dependency:
        return (ROOT / 'pishoo' / dependency['path']).resolve() / 'src/migrations'
    packages = tomllib.loads((ROOT / 'Cargo.lock').read_text())['package']
    source = next(package['source'] for package in packages
                  if package['name'] == 'daccess')
    revision = source.rsplit('#', 1)[1]
    cargo_home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))).expanduser()
    for checkout in (cargo_home / 'git/checkouts').glob(f'daccess-*/{revision[:7]}'):
        migrations = checkout / 'src/migrations'
        if (migrations / '2.sql').is_file():
            return migrations
    raise RuntimeError('先运行 cargo fetch --locked，或设置 DACCESS_PATH 指向 daccess 源码目录')


MIGRATIONS = daccess_migrations()


class ContactMigration(unittest.TestCase):
    def database(self, version=1):
        db = sqlite3.connect(':memory:')
        self.addCleanup(db.close)
        for step in range(version + 1):
            db.executescript((MIGRATIONS / f'{step}.sql').read_text())
        db.execute('UPDATE module SET version=?', (version,))
        return db

    def contact(self, db, name='alice.dhttp.net'):
        db.execute('INSERT INTO contacts(name,subject_id,status,expired_after,updated_at,created_at) VALUES(?, ?, 2, 2000000000, 100, 100)', (name, b'a'))

    def rule(self, db, method, path, effect, grantee='**', kind=2):
        db.execute('INSERT INTO access_rules(method,api,effect,grantee_type,grantee,updated_at,created_at) VALUES(?,?,?,?,?,100,100)', (method, path, effect, kind, grantee))

    def migrate(self, db):
        version = db.execute('SELECT version FROM module').fetchone()[0]
        db.commit()
        for step in range(version + 1, 3):
            db.executescript('BEGIN;\n' + (MIGRATIONS / f'{step}.sql').read_text()
                             + f'\nUPDATE module SET version={step};\nCOMMIT;')

    def test_fresh_database_has_no_default_acl_rules(self):
        db = self.database(0)
        self.migrate(db)
        self.assertEqual(db.execute('SELECT version FROM module').fetchone(), (2,))
        for table in ['contacts', 'access_rules', 'access_reviews', 'contact_applications', 'contact_deletions']:
            self.assertEqual(db.execute(f'SELECT count(*) FROM {table}').fetchone(), (0,))
        self.assertEqual(db.execute('PRAGMA integrity_check').fetchone(), ('ok',))

    def test_upgrade_preserves_contacts_and_acl_without_converting_paths(self):
        for version in [0, 1]:
            with self.subTest(version=version):
                db = self.database(version)
                self.contact(db)
                self.rule(db, 'POST', '/std/contact', 'allow')
                self.rule(db, 'POST', '/std/contact/application', 'deny')
                self.rule(db, 'GET', '/std/contact/self', 'deny')
                self.rule(db, 'GET', '/std/contact/local/saved', 'deny')
                self.rule(db, 'GET', '/std/acl/review/12/status', 'deny', 'alice.dhttp.net', 0)
                contacts = db.execute('SELECT * FROM contacts').fetchall()
                rules = db.execute('SELECT * FROM access_rules ORDER BY id').fetchall()
                self.migrate(db)
                self.assertEqual(db.execute('SELECT * FROM contacts').fetchall(), contacts)
                self.assertEqual(db.execute('SELECT * FROM access_rules ORDER BY id').fetchall(), rules)
                self.assertEqual(db.execute('SELECT version FROM module').fetchone(), (2,))
                self.assertEqual(db.execute('PRAGMA integrity_check').fetchone(), ('ok',))

    def test_upgrade_preserves_reviews_and_application_receipts(self):
        db = self.database()
        db.execute("INSERT INTO access_reviews(request_id,visitor,visitor_sid,method,api,stage,reason,expired_after,updated_at,created_at) VALUES('r1','alice.dhttp.net',X'01','POST','/private',0,'review',2000000000,100,100)")
        db.execute("INSERT INTO contact_applications(application_id,applicant_name,applicant_sid,payload_hash,status,received_at,expired_after) VALUES(?, 'alice.dhttp.net',X'01',?,'pending',100,2000000000)", ('a' * 64, 'b' * 64))
        reviews = db.execute('SELECT * FROM access_reviews').fetchall()
        applications = db.execute('SELECT * FROM contact_applications').fetchall()
        self.migrate(db)
        self.assertEqual(db.execute('SELECT * FROM access_reviews').fetchall(), reviews)
        self.assertEqual(db.execute('SELECT * FROM contact_applications').fetchall(), applications)

    def test_timestamps_deletions_duplicate_aliases_and_contact_generation(self):
        db = self.database()
        self.contact(db)
        db.execute("UPDATE sqlite_sequence SET seq=1000 WHERE name='contacts'")
        self.migrate(db)
        self.contact(db, 'bob.dhttp.net')
        self.assertGreater(db.execute("SELECT id FROM contacts WHERE name='bob.dhttp.net'").fetchone()[0], 1000)
        db.execute("UPDATE contacts SET alias='Alex', expired_after=101")
        db.execute("UPDATE contacts SET description='Changed after expiry'")
        self.assertTrue(all(row[0] > 100 for row in db.execute('SELECT updated_at FROM contacts')))
        self.rule(db, 'GET', '/std/message', 'allow', 'alice.dhttp.net', 0)
        db.execute("DELETE FROM contacts WHERE name='alice.dhttp.net'")
        self.assertEqual(db.execute('SELECT name FROM contact_deletions').fetchall(), [('alice.dhttp.net',)])
        self.assertFalse(db.execute("SELECT 1 FROM access_rules WHERE grantee='alice.dhttp.net'").fetchall())
        self.contact(db)
        self.assertFalse(db.execute('SELECT 1 FROM contact_deletions').fetchall())

    def test_migration_failure_rolls_back_structure_and_data(self):
        db = self.database()
        self.contact(db)
        self.rule(db, 'POST', '/private', 'deny')
        db.execute('CREATE TABLE contact_deletions(marker TEXT)')
        db.execute("INSERT INTO contact_deletions VALUES('keep')")
        schema = db.execute("SELECT sql FROM sqlite_master WHERE name='contacts'").fetchone()
        contacts = db.execute('SELECT * FROM contacts').fetchall()
        rules = db.execute('SELECT * FROM access_rules').fetchall()
        with self.assertRaises(sqlite3.OperationalError):
            self.migrate(db)
        db.rollback()
        self.assertEqual(db.execute('SELECT version FROM module').fetchone(), (1,))
        self.assertEqual(db.execute("SELECT sql FROM sqlite_master WHERE name='contacts'").fetchone(), schema)
        self.assertEqual(db.execute('SELECT * FROM contacts').fetchall(), contacts)
        self.assertEqual(db.execute('SELECT * FROM access_rules').fetchall(), rules)
        self.assertEqual(db.execute('SELECT * FROM contact_deletions').fetchall(), [('keep',)])

    def test_new_delivery_schema_has_no_saved_collection(self):
        db = sqlite3.connect(':memory:')
        self.addCleanup(db.close)
        db.executescript((ROOT / 'pishoo/src/contact/contact.sql').read_text())
        self.assertFalse(db.execute("SELECT 1 FROM sqlite_master WHERE name='saved_contacts'").fetchall())
        columns = {row[1] for row in db.execute('PRAGMA table_info(outbound_contact_request)')}
        self.assertTrue({'contact_id', 'contact_subject'} <= columns)


if __name__ == '__main__':
    unittest.main()

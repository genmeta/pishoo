#!/usr/bin/env python3
"""Verify real old-package migration, failed-update retention and CLI update."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess


def facts(root):
    profile=root/'home/host.sandbox'
    result={}
    for name,tables in [('.volume/std/profile/database',['profile_fields']),('.volume/std/message/database',['chat_conversations','chat_messages','chat_jobs']),('.volume/sys/access/database',['access_rules','contacts']),('.volume/sys/contact/database',['outbound_contact_request'])]:
        db=sqlite3.connect(profile/name)
        result[name]={table:db.execute(f'SELECT * FROM {table} ORDER BY 1').fetchall() for table in tables}
        db.close()
    result['attachment']=hashlib.sha256((profile/'.volume/std/profile/.file/kept.txt').read_bytes()).hexdigest()
    return result


def packages(root):
    db=sqlite3.connect(root/'home/host.sandbox/.volume/sys/pkg/database')
    result=db.execute('SELECT name,author,version FROM package ORDER BY name').fetchall()
    assert db.execute('PRAGMA user_version').fetchone()[0]==4
    db.close()
    return result


def run(root):
    spec=importlib.util.spec_from_file_location('control',root/'control.py');control=importlib.util.module_from_spec(spec);spec.loader.exec_module(control)
    control.reset()
    initial=packages(root)
    assert {name:version for name,_,version in initial}=={'profile':'0.3.0','message':'0.2.1','introduction':'0.1.1','workspace':'0.3.5'}
    assert all(author=='dev.genmeta.tech.dhttp.net' for _,author,_ in initial)
    before=facts(root)
    env=dict(os.environ,DHTTP_HOME=str(root/'client-home'))
    command=[str(root/'build/cargo/debug/genmeta'),'update','workspace','--id','host.sandbox']
    artifact=root/'workspace-0.3.6.waz';digest=hashlib.sha256(artifact.read_bytes()).hexdigest()
    saved=next((root/'home/store').rglob(digest+'.waz'))
    original=saved.read_bytes()
    try:
        damaged=bytearray(original);damaged[len(damaged)//2]^=1;saved.write_bytes(damaged)
        rejected=subprocess.run(command,env=env,capture_output=True,text=True,timeout=120)
        (root/'logs/failed-update.log').write_text(rejected.stdout+rejected.stderr)
        assert rejected.returncode!=0,'tampered package unexpectedly installed'
        assert packages(root)==initial,'failed update changed package registrations'
        assert facts(root)==before,'failed update changed business data or ACLs'
    finally:
        saved.write_bytes(original)
    updated=subprocess.run(command,env=env,capture_output=True,text=True,timeout=120)
    (root/'logs/verified-cli-update.log').write_text(updated.stdout+updated.stderr)
    assert updated.returncode==0,updated.stderr
    assert {name:version for name,_,version in packages(root)}=={'profile':'0.3.1','message':'0.2.2','introduction':'0.1.2','workspace':'0.3.6'}
    assert facts(root)==before,'successful update changed business data or ACLs'
    repeated=subprocess.run(command,env=env,capture_output=True,text=True,timeout=30)
    assert repeated.returncode==0 and json.loads(repeated.stdout)['unchanged'] is True
    control.stop(['gateway','pishoo']);control.start()
    assert facts(root)==before,'restart changed business data or ACLs'
    report={'old_package_schema':3,'current_package_schema':4,'old_versions':initial,'updated_versions':packages(root),
            'rejected_tampered_release':True,'cli_update':True,'repeat_unchanged':True,'restart':True,
            'profile_message_contacts_acl_attachment_unchanged':True,
            'before':before}
    (root/'cli-acceptance.json').write_text(json.dumps(report,indent=2,ensure_ascii=False,default=str)+'\n')
    print('PASS: schema v3 migration, tampered-package rollback, full CLI closure, repeat, restart and all retained data.')


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('environment',type=Path);run(parser.parse_args().environment.resolve())

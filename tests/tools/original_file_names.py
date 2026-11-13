#!/usr/bin/env python3
"""Explicit, stopped-host conversion of Message archive files to original names.
Keeps API IDs and message text unchanged. Backups and the move plan are mandatory.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess


def filename(value):
    if not isinstance(value, str) or not value or value in ('.', '..') or len(value.encode()) > 255 or any(c in value for c in '/\\\0') or any(ord(c) < 32 or ord(c) == 127 for c in value):
        raise ValueError(f'Unsafe original filename: {value!r}')
    return value


def under(root, relative):
    if not isinstance(relative, str) or not relative or relative.startswith('/'):
        raise ValueError('Invalid archive path')
    parts = relative.split('/')
    for part in parts: filename(part)
    path = root
    for part in parts:
        path = path / part
        if path.is_symlink(): raise ValueError(f'Symlink is not a migration source: {path}')
    return path


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as file:
        while data := file.read(1024 * 1024): digest.update(data)
    return digest.hexdigest()


def suffix(name, number):
    if number == 1: return name
    stem, dot, extension = name.rpartition('.')
    if not stem or not extension or len(extension.encode()) > 32: stem, dot, extension = name, '', ''
    tail = f' ({number}){dot}{extension}'
    limit = 255 - len(tail.encode())
    while len(stem.encode()) > limit: stem = stem[:-1]
    return stem + tail


def plan_volume(volume):
    root = volume / '.file'
    if root.is_symlink(): raise ValueError('File root is a symlink')
    moves, reserved = [], set()
    db_path = volume / ('message.db' if (volume / 'message.db').exists() else 'database')
    # Read historical references first so their immutable original names stay exact.
    if db_path.exists():
        with sqlite3.connect(f'file:{db_path}?mode=ro&vfs=unix-dotfile', uri=True) as db:
            records = db.execute("SELECT contact_name,text FROM chat_messages WHERE media_type='application/vnd.pishoo.file+json' ORDER BY id").fetchall()
        seen = {}
        for peer, text in records:
            value = json.loads(text)
            if value.get('version') != 1: continue
            name = filename(value['name'])
            source = under(root, f"{peer}/{value['day']}/{value['blake3']}")
            if source.is_dir():
                marker = json.loads((source / 'complete.json').read_text())
                if marker.get('size') != value['size']: raise ValueError('Incomplete legacy archive')
                source = under(root, str((source / 'data').relative_to(root)))
            target = under(root, f"{peer}/{value['day']}/{name}")
            if not source.exists():
                if target.is_file(): reserved.add(str(target)); continue
                # A historical description without a saved file stays unavailable.
                continue
            if not source.is_file() or source.stat().st_size != value['size']: raise ValueError(f'Legacy size differs: {source}')
            key = str(target)
            digest = sha256(source)
            if key in seen:
                if seen[key] != digest: raise ValueError(f'Ambiguous legacy filename: {target}')
                continue
            seen[key] = digest
            if target.exists() and not os.path.samefile(source, target): raise ValueError(f'Legacy target already exists: {target}')
            reserved.add(key)
            if source != target:
                moves.append({'source':str(source.relative_to(root)), 'target':str(target.relative_to(root)), 'sha256':digest, 'id':None})
    native = root / 'database'
    if native.exists():
        if native.is_symlink(): raise ValueError('Index is a symlink')
        with sqlite3.connect(f'file:{native}?mode=ro', uri=True) as db:
            db.row_factory = sqlite3.Row
            has_name = any(r[1] == 'storage_name' for r in db.execute('PRAGMA table_info(files)'))
            rows = db.execute('SELECT id,directory,name,size,offset,' + ('storage_name' if has_name else 'NULL AS storage_name') + ' FROM files ORDER BY created_at,id').fetchall()
        for value in rows:
            if value['storage_name'] is not None:
                reserved.add(str(under(root, value['directory'] + '/' + filename(value['storage_name'])))); continue
            if not re.fullmatch('[0-9a-f]{32}', value['id']): raise ValueError('Invalid resource id')
            source = under(root, f"{value['directory']}/{value['id']}/content")
            if not source.is_file() or source.stat().st_size < value['offset']: raise ValueError(f'Missing committed bytes: {source}')
            name = filename(value['name'])
            for number in range(1,10001):
                target = under(root, value['directory'] + '/' + suffix(name, number))
                if not target.exists() and str(target) not in reserved: break
            else: raise ValueError('Too many equal filenames')
            reserved.add(str(target))
            moves.append({'source':str(source.relative_to(root)), 'target':str(target.relative_to(root)), 'sha256':sha256(source), 'id':value['id']})
    return moves


def apply_volume(volume, backup):
    if backup.exists(): raise ValueError('Backup destination already exists')
    moves = plan_volume(volume)
    shutil.copytree(volume, backup)
    (backup / 'original-name-plan.json').write_text(json.dumps(moves,ensure_ascii=False,indent=2)+'\n')
    root = volume / '.file'
    created = []
    native = root / 'database'
    db = sqlite3.connect(native) if native.exists() else None
    try:
        if db:
            db.execute('BEGIN IMMEDIATE')
            if not any(r[1] == 'storage_name' for r in db.execute('PRAGMA table_info(files)')): db.execute('ALTER TABLE files ADD COLUMN storage_name TEXT')
        for move in moves:
            source, target = under(root, move['source']), under(root, move['target'])
            if sha256(source) != move['sha256']: raise ValueError('Source changed after planning')
            if not target.exists():
                os.link(source, target, follow_symlinks=False)
                created.append(target)
            elif not os.path.samefile(source,target): raise ValueError('Target appeared after planning')
            fd=os.open(target.parent, os.O_RDONLY)
            try: os.fsync(fd)
            finally: os.close(fd)
            if move['id'] is not None:
                changed=db.execute('UPDATE files SET storage_name=? WHERE id=? AND storage_name IS NULL',(target.name,move['id'])).rowcount
                if changed!=1: raise ValueError('Resource changed after planning')
        if db: db.commit()
    except BaseException:
        if db: db.rollback()
        for path in created: path.unlink()
        raise
    finally:
        if db: db.close()
    # The new index is durable and all historical names exist before old names go.
    for source in {under(root, m['source']) for m in moves}:
        source.unlink()
        if source.name in ('content','data'):
            for name in ('complete.json','envelope.json','state.json'):
                old=source.parent/name
                if old.is_file() and not old.is_symlink(): old.unlink()
            try: source.parent.rmdir()
            except OSError: pass
    # Only remove empty obsolete resource directories, never unknown content.
    for archive in root.glob('*/*'):
        if archive.is_dir() and not archive.is_symlink():
            for child in archive.iterdir():
                if child.is_dir() and not child.is_symlink() and re.fullmatch('[0-9a-f]{32}',child.name):
                    try: child.rmdir()
                    except OSError: pass
    for move in moves:
        if sha256(under(root,move['target']))!=move['sha256']: raise ValueError('Post-migration bytes differ')
    return moves


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('profiles',nargs='+',type=Path)
    parser.add_argument('--apply',action='store_true')
    parser.add_argument('--backup',type=Path)
    args=parser.parse_args()
    if args.apply:
        if not args.backup: parser.error('--backup is required')
        if subprocess.run(['pgrep','-x','pishoo'],capture_output=True).returncode==0: parser.error('Stop Pishoo before --apply')
    for profile in args.profiles:
        volume=profile/'.volume/std/message'
        moves=apply_volume(volume,args.backup/profile.name) if args.apply else plan_volume(volume)
        print(json.dumps({'profile':profile.name,'moves':moves},ensure_ascii=False,indent=2))

#!/usr/bin/env python3
"""Prepare reviewable host transfer authorizations from audited original releases.

This is an offline release tool, not a runtime override. It verifies historical
package bytes, identity, signature and chain. Targets must be independently
reviewed; generating JSON does not itself authorize a production transfer.
"""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess
import tempfile
import zipfile


def prepare(metadata_path, package_path, target, root, openssl):
    metadata=json.loads(metadata_path.read_text())
    data=package_path.read_bytes()
    if hashlib.sha256(data).hexdigest()!=metadata['package_sha256']:
        raise ValueError('original release digest mismatch')
    author,name,version=(metadata[key] for key in ('author','name','version'))
    if not re.fullmatch(r'[a-z][a-z0-9-]{0,62}',name):raise ValueError('invalid package name')
    for value in [metadata['author_owner_hash'],target['owner_hash']]:
        if not re.fullmatch('[0-9a-f]{64}',value):raise ValueError('reviewed owner hash required')
    if not target['evidence'].strip():raise ValueError('target ownership evidence required')
    with zipfile.ZipFile(package_path) as archive:
        names=archive.namelist()
        if len(set(names))!=len(names) or len(names)>4096:raise ValueError('ambiguous original archive')
        if sum(item.file_size for item in archive.infolist())>64*1024*1024:raise ValueError('original archive exceeds budget')
        contents={path:archive.read(path) for path in names}
    manifest=json.loads(contents['manifest.json'])
    if any(manifest['package'][key]!=metadata[key] for key in ('author','name','version')):
        raise ValueError('original manifest identity mismatch')
    envelope=json.loads(contents['signature.json'])
    if envelope['certificates']!=metadata['signer_certificates']:raise ValueError('original signer chain mismatch')
    if not 2<=len(envelope['certificates'])<=3:raise ValueError('unexpected original chain')
    payload=hashlib.sha256(b'pishoo-waz-payload-v1\0')
    for path,content in sorted(contents.items()):
        if path=='signature.json':continue
        encoded=path.encode();payload.update(struct.pack('<I',len(encoded))+encoded+struct.pack('<Q',len(content))+hashlib.sha256(content).digest())
    def command(*args,data=None):
        return subprocess.run([openssl,*map(str,args)],input=data,capture_output=True,check=True).stdout
    with tempfile.TemporaryDirectory(prefix='legacy-audit-') as directory:
        directory=Path(directory)
        for index,cert in enumerate(envelope['certificates'][:2]):
            (directory/f'{index}.pem').write_bytes(command('x509','-inform','DER','-outform','PEM',data=base64.b64decode(cert,validate=True)))
        leaf=directory/'0.pem';issuer=directory/'1.pem'
        command('verify','-CAfile',root,'-untrusted',issuer,'-attime',str(metadata['published_at']),'-verify_hostname',author,leaf)
        ski=command('x509','-in',leaf,'-noout','-ext','subjectKeyIdentifier').decode().splitlines()[1].strip()
        if bytes.fromhex(ski.replace(':','')).decode().split(':')[-1]!=metadata['author_owner_hash']:
            raise ValueError('historical owner does not match certificate')
        public=directory/'public.pem';public.write_bytes(command('x509','-in',leaf,'-pubkey','-noout'))
        signature=directory/'signature.der';signature.write_bytes(base64.b64decode(envelope['signature'],validate=True))
        command('dgst','-sha384','-verify',public,'-signature',signature,data=b'pishoo-waz-signature-v1\0'+payload.digest())
    standard=author=='std.dhttp.net'
    files={}
    for path,content in contents.items():
        if path=='backend.wasm':path=f'lib/{"std/" if standard else ""}{name}.wasm'
        elif path.startswith('public/'):path=f'app/{name}/'+path.removeprefix('public/')
        else:continue
        if any(part in ('','.','..') for part in path.split('/')) or '\\' in path:raise ValueError('unsafe runtime path')
        files[path]={'sha256':hashlib.sha256(content).hexdigest(),'size':len(content)}
    if not files:raise ValueError('release has no runtime files')
    return {'author':author,'name':name,'version':version,'author_owner_hash':metadata['author_owner_hash'],
            'package_sha256':metadata['package_sha256'],'target_author':target['author'],'target_owner_hash':target['owner_hash'],
            'evidence':target['evidence'],'files':files}


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release',nargs=2,type=Path,action='append',required=True,metavar=('METADATA','WAZ'))
    parser.add_argument('--targets',type=Path,required=True)
    parser.add_argument('--root',type=Path,required=True)
    parser.add_argument('--openssl',default='/opt/homebrew/bin/openssl')
    parser.add_argument('--group',required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();targets=json.loads(args.targets.read_text())
    entries=[]
    for metadata,waz in args.release:
        author=json.loads(metadata.read_text())['author']
        entries.append(prepare(metadata,waz,targets[author],args.root,args.openssl))
    with args.output.open('x') as stream:json.dump([{'group':args.group,'packages':entries}],stream,indent=2);stream.write('\n')

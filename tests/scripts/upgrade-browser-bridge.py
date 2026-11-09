#!/usr/bin/env python3
"""Loopback browser bridge using the real isolated genmeta DHTTP/3 client.

Each request resolves through mDNS and authenticates with the test host's real
credentials. Store reads use the same host identity directly against Registry.
Only disposable test homes are accepted; normal user credentials are never read.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import urlsplit

ROOT=Path(os.environ['UPGRADE_ENV']).resolve()
CLIENT=ROOT/'build/cargo/debug/genmeta'
ENV=dict(os.environ,DHTTP_HOME=str(ROOT/'client-home'))
HOST='host.sandbox.dhttp.net'
HEADERS=['content-type','pishoo-version','package-id','pishoo-viewer-name','cache-control','location']


class Bridge(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'

    def handle_request(self):
        if self.headers.get('origin') not in (None,'http://127.0.0.1:18082','http://host.sandbox.dhttp.net:18082'):
            self.send_error(403);return
        length=int(self.headers.get('content-length','0'))
        if not 0<=length<=64*1024*1024:
            self.send_error(413);return
        body=self.rfile.read(length)
        target=HOST;path=self.path
        if path.startswith('/std/forward/store.dhttp.net/'):
            target='store.dhttp.net';path=path.removeprefix('/std/forward/store.dhttp.net')
        with tempfile.TemporaryDirectory(prefix='upgrade-http-',dir=ROOT/'requests') as directory:
            directory=Path(directory);output=directory/'response';upload=directory/'request'
            command=[str(CLIENT),'curl','--id',HOST,'--dns','h3','--connect-timeout','8','-sS','-X',self.command,'-o',str(output),'-w','%{response_code}\n'+''.join('%{header{'+name+'}}\n' for name in HEADERS)]
            for key,value in self.headers.items():
                if key.lower() not in ('host','connection','transfer-encoding','content-length','origin','referer','accept-encoding'):
                    command+=['-H',key+': '+value]
            if body:
                upload.write_bytes(body);command+=['-T',str(upload)]
            command+=[f'https://{target}{path}']
            try:
                result=subprocess.run(command,env=ENV,capture_output=True,timeout=180)
                if result.returncode:
                    self.send_error(502,'Authenticated DHTTP request failed');return
                metadata=result.stdout.decode().splitlines();status=int(metadata[0]);content=output.read_bytes() if output.exists() else b''
                self.send_response(status)
                for key,value in zip(HEADERS,metadata[1:]):
                    if value and value!='0':self.send_header(key,value)
                self.send_header('content-length',str(len(content)))
                self.end_headers()
                if self.command!='HEAD':self.wfile.write(content)
            except (subprocess.TimeoutExpired,ValueError,OSError,IndexError):
                self.send_error(502,'DHTTP response unavailable; inspect the isolated service logs')

    do_GET=handle_request
    do_HEAD=handle_request
    do_POST=handle_request
    do_PUT=handle_request
    do_PATCH=handle_request
    do_DELETE=handle_request


if __name__=='__main__':
    (ROOT/'requests').mkdir(mode=0o700,exist_ok=True)
    print('Browser bridge: http://127.0.0.1:18082/app/workspace/',flush=True)
    ThreadingHTTPServer(('127.0.0.1',18082),Bridge).serve_forever()

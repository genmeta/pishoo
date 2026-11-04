#!/usr/bin/env python3
"""Generate valid, isolated TEST ONLY TLS/developer certificates and OCSP staples.

No production credentials, CertServer, system trust store or DNS overrides are used.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

OPENSSL = os.environ.get("OPENSSL", "/opt/homebrew/bin/openssl")


def run(*args, data=None):
    return subprocess.run([OPENSSL, *map(str,args)],input=data,check=True,capture_output=True).stdout


def prepare(destination):
    destination.mkdir(parents=True,exist_ok=False)
    destination.chmod(0o700)
    pki=destination/"pki";pki.mkdir()
    home=destination/"home";home.mkdir()
    run("genpkey","-algorithm","EC","-pkeyopt","ec_paramgen_curve:P-384","-out",pki/"root.key")
    run("req","-new","-x509","-sha384","-days","365","-key",pki/"root.key","-subj","/CN=Upgrade Environment TEST ONLY Root","-addext","basicConstraints=critical,CA:TRUE,pathlen:1","-addext","keyUsage=critical,keyCertSign,cRLSign","-addext","subjectKeyIdentifier=hash","-out",pki/"root.crt")
    run("genpkey","-algorithm","EC","-pkeyopt","ec_paramgen_curve:P-384","-out",pki/"issuer.key")
    run("req","-new","-sha384","-key",pki/"issuer.key","-subj","/CN=Upgrade Environment TEST ONLY Unified Developer Issuer","-out",pki/"issuer.csr")
    (pki/"issuer.ext").write_text("basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,digitalSignature,keyCertSign,cRLSign\nextendedKeyUsage=serverAuth,clientAuth,codeSigning\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\n")
    run("x509","-req","-sha384","-days","180","-in",pki/"issuer.csr","-CA",pki/"root.crt","-CAkey",pki/"root.key","-set_serial","100","-extfile",pki/"issuer.ext","-out",pki/"issuer.crt")
    identities={}
    for number,name in enumerate(["host.sandbox.dhttp.net","store.dhttp.net","dev.genmeta.tech.dhttp.net","dev.alice.smith.dhttp.net"],start=101):
        owner=hashlib.sha256(("TEST ONLY upgrade owner "+name).encode()).hexdigest()
        profile=home/name.removesuffix(".dhttp.net");ssl=profile/"ssl";ssl.mkdir(parents=True)
        key=ssl/"privkey.pem";leaf=ssl/"leaf.crt"
        run("genpkey","-algorithm","EC","-pkeyopt","ec_paramgen_curve:P-384","-out",key)
        run("req","-new","-sha384","-key",key,"-subj",f"/CN={name}","-out",ssl/"request.csr")
        ski=f"1:0:{owner}";encoded=":".join(f"{b:02x}" for b in ski.encode())
        eku="serverAuth,clientAuth,codeSigning" if name.startswith("dev.") else "serverAuth,clientAuth"
        ext=ssl/"leaf.ext";ext.write_text(f"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage={eku}\nsubjectAltName=DNS:{name}\nsubjectKeyIdentifier={encoded}\nauthorityKeyIdentifier=keyid:always\n")
        run("x509","-req","-sha384","-days","30","-in",ssl/"request.csr","-CA",pki/"issuer.crt","-CAkey",pki/"issuer.key","-set_serial",str(number),"-extfile",ext,"-out",leaf)
        (ssl/"fullchain.crt").write_bytes(leaf.read_bytes()+(pki/"issuer.crt").read_bytes())
        expiry=run("x509","-in",leaf,"-noout","-enddate").decode()
        import datetime as dt
        end=dt.datetime.strptime(expiry.strip().split("=",1)[1],"%b %d %H:%M:%S %Y %Z").strftime("%y%m%d%H%M%SZ")
        index=ssl/"index.txt";index.write_text(f"V\t{end}\t\t{number:X}\tunknown\t/CN={name}\n")
        run("ocsp","-rmd","sha384","-index",index,"-rsigner",pki/"issuer.crt","-rkey",pki/"issuer.key","-CA",pki/"issuer.crt","-issuer",pki/"issuer.crt","-cert",leaf,"-respout",ssl/"stapling.ocsp","-ndays","7")
        run("verify","-CAfile",pki/"root.crt","-untrusted",pki/"issuer.crt","-verify_hostname",name,leaf)
        run("ocsp","-respin",ssl/"stapling.ocsp","-issuer",pki/"issuer.crt","-cert",leaf,"-CAfile",pki/"root.crt","-no_nonce")
        # Registry retains its pinned dhttp-home 0.6.0 cache filename.
        (ssl/"ocsp.der").write_bytes((ssl/"stapling.ocsp").read_bytes())
        key.chmod(0o400)
        if name.startswith("dev."):
            folder=profile/"code-signing/1";folder.mkdir(parents=True)
            # This is the same on-disk credential envelope consumed by Jade.
            (folder/"credentials.json").write_text(json.dumps({"cert_pem":(ssl/"fullchain.crt").read_text(),"key_pem":key.read_text()}))
            (folder/"credentials.json").chmod(0o600)
            (profile/"trust/code-signing").mkdir(parents=True)
            # Unified developer transport reuses a separately cached, verified staple.
        identities[name]={"owner_hash":owner,"profile":str(profile)}
    for key in pki.glob("*.key"):key.chmod(0o600)
    (destination/"identities.json").write_text(json.dumps(identities,indent=2)+"\n")
    print("Prepared isolated certificates, private test home and signed OCSP responses:",destination)


if __name__=="__main__":
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument("destination",type=Path)
    prepare(parser.parse_args().destination.resolve())

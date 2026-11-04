# Public TEST ONLY code-signing vectors

Certificates/status examples are copied from the shared isolated generator at
the shared `pishoo-projects/integration/code-signing/generate.py` (CertServer baseline ce3518f).
No private keys are present. Production builds only use the compiled DHTTP Root;
the additional explicit test anchor is confined to cfg(test).

`jade-developer.waz` is the real Jade-built static WAZ
`developer.dhttp.net/developer-jade-smoke@0.0.1`, built with a signing home that
contained no TLS ssl directory. Complete original ZIP SHA256:
`ec726212c8c8be0081360eddf44a60d06e76a6ba1958d65bca3e16532dddab8e`.
The same bytes are consumed by Registry and Pishoo acceptance tests.

`jade-registry-proof.mock.json` is an explicit mock of the authenticated Registry
publication record, bound to that fixed package hash and the shared certificate
instance. The tests sign portable publication receipts with an isolated Registry test
identity before installation. Historical status JSON files are public protocol
vectors, not current-state evidence or an online dependency. These fixtures test
installation/registration policy and package interoperability; they do not assert
a real Registry publication or production payment/signing deployment.

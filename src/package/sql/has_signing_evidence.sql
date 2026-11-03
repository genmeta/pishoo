SELECT EXISTS(
 SELECT 1 FROM pragma_table_info('package','pkg_registry')
 WHERE name='signer_leaf_fingerprint_sha256'
)

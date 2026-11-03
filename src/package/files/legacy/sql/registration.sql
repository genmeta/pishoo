SELECT author,version,author_owner_hash,package_sha256,signer_leaf_fingerprint_sha256,has_lib,has_app FROM package WHERE name=?1;

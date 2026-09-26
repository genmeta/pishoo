fn load_libs(
    profile: &IdentityProfile,
    runtime: &Arc<Runtime>,
    old: &BTreeMap<String, Arc<Lib>>,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, Arc<Lib>>> {
    let root = profile.join("lib");
    let mut candidates = BTreeMap::new();
    match root.symlink_metadata() {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(Error::InvalidComponent(
                "lib root must be a directory, not a symlink".into(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(e) => return Err(e.into()),
        _ => {}
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(e) => return Err(e.into()),
    };
    let mut entries = entries.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if id.is_empty()
            || id.len() > 63
            || !id.as_bytes()[0].is_ascii_lowercase()
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            continue;
        }
        let path = entry.path().join("lib.wasm");
        let metadata = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let result = (|| -> Result<Arc<Lib>> {
            if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 * 1024 {
                return Err(Error::InvalidComponent("invalid component file".into()));
            }
            let bytes = std::fs::read(&path)?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if let Some(lib) = old.get(&id).filter(|lib| lib.digest == digest) {
                return Ok(lib.clone());
            }
            let token = old
                .get(&id)
                .map_or_else(|| cancel.child_token(), |lib| lib.cancel.clone());
            let lib = Lib::load(
                runtime.clone(),
                id.clone(),
                &bytes,
                &entry.path().join("data"),
                LibPolicy::default(),
                token,
            )?;
            Ok(Arc::new(lib))
        })();
        match result {
            Ok(lib) => {
                candidates.insert(id, lib);
            }
            Err(e) => {
                eprintln!("keeping/skipping lib {id}: {e}");
                if let Some(lib) = old.get(&id) {
                    candidates.insert(id, lib.clone());
                }
            }
        }
    }
    Ok(candidates)
}

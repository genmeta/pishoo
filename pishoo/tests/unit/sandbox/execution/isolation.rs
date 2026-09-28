use super::*;

#[test]
fn memory_limit_counts_all_memories_and_rolls_back_failed_growth() {
    let mut limit = MemoryLimits {
        base: StoreLimitsBuilder::new().table_elements(100_000).build(),
        used: 0,
        pending: 0,
    };
    assert!(limit.memory_growing(0, 32 << 20, None).unwrap());
    assert!(limit.memory_growing(0, 32 << 20, None).unwrap());
    limit
        .memory_grow_failed(wasmtime::Error::msg("allocation failed"))
        .unwrap();
    assert_eq!(limit.used, 32 << 20);
    assert!(limit.memory_growing(0, 32 << 20, None).unwrap());
    assert!(!limit.memory_growing(0, 65536, None).unwrap());
    assert!(!limit.table_growing(0, 100_001, None).unwrap());
}

#[tokio::test]
async fn filesystem_grants_are_private_read_only_and_hold_the_open_directory() {
    use wasmtime::component::Resource;
    use wasmtime_wasi::{
        filesystem::WasiFilesystemCtxView,
        p2::bindings::filesystem::{
            preopens::Host,
            types::{DescriptorFlags, HostDescriptor, OpenFlags, PathFlags},
        },
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("first")).unwrap();
    std::fs::create_dir(root.path().join("second")).unwrap();
    std::fs::write(root.path().join("first/value"), "first").unwrap();
    std::fs::write(root.path().join("second/value"), "secret sibling").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        root.path().join("second/value"),
        root.path().join("first/link"),
    )
    .unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let lib = Lib::load(
        runtime,
        "test".into(),
        &component(READ),
        &root.path().join("first"),
        LibPolicy {
            data_write: false,
            ..LibPolicy::default()
        },
        CancellationToken::new(),
    )
    .unwrap();
    std::fs::rename(root.path().join("first"), root.path().join("saved")).unwrap();
    std::fs::create_dir(root.path().join("first")).unwrap();
    std::fs::write(root.path().join("first/value"), "replacement").unwrap();
    let mut table = ResourceTable::new();
    let mut filesystem = lib.filesystem.clone();
    let mut view = WasiFilesystemCtxView {
        ctx: &mut filesystem,
        table: &mut table,
    };
    let dirs = Host::get_directories(&mut view).unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].1, "/data");
    let descriptor = dirs[0].0.rep();
    let file = view
        .open_at(
            Resource::new_borrow(descriptor),
            PathFlags::empty(),
            "value".into(),
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
        .unwrap();
    assert_eq!(view.read(file, 100, 0).await.unwrap().0, b"first");
    assert!(
        view.open_at(
            Resource::new_borrow(descriptor),
            PathFlags::empty(),
            "value".into(),
            OpenFlags::empty(),
            DescriptorFlags::WRITE
        )
        .await
        .is_err()
    );
    for path in ["../second/value", "/second/value", "link"] {
        assert!(
            view.open_at(
                Resource::new_borrow(descriptor),
                PathFlags::SYMLINK_FOLLOW,
                path.into(),
                OpenFlags::empty(),
                DescriptorFlags::READ
            )
            .await
            .is_err()
        );
    }
}

#[test]
fn real_store_cannot_allocate_more_than_the_combined_memory_limit() {
    let engine = Engine::default();
    let mut store = Store::new(
        &engine,
        MemoryLimits {
            base: StoreLimitsBuilder::new().memory_size(64 << 20).build(),
            used: 0,
            pending: 0,
        },
    );
    store.limiter(|limits| limits);
    let memory = wasmtime::MemoryType::new(512, None);
    let _first = wasmtime::Memory::new(&mut store, memory.clone()).unwrap();
    let _second = wasmtime::Memory::new(&mut store, memory).unwrap();
    assert_eq!(store.data().used, 64 << 20);
    assert!(wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None)).is_err());
}

#[test]
fn lib_ids_follow_the_deployment_grammar() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    for id in [
        "",
        "Upper",
        "with_underscore",
        "1number",
        "../other",
        &"a".repeat(64),
    ] {
        let error = Lib::load(
            runtime.clone(),
            id.into(),
            &[],
            directory.path(),
            LibPolicy::default(),
            CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::InvalidComponent(message) if message == "invalid Lib id"));
    }
}

#[cfg(unix)]
#[test]
fn data_directory_symlinks_cannot_grant_sibling_or_identity_files() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let bytes = component(READ);
    for target in ["ssl", "db", "sibling"] {
        let target = root.path().join(target);
        std::fs::create_dir(&target).unwrap();
        let data = root.path().join("data");
        std::os::unix::fs::symlink(&target, &data).unwrap();
        let error = Lib::load(
            runtime.clone(),
            "test".into(),
            &bytes,
            &data,
            LibPolicy::default(),
            CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::InvalidComponent(_)));
        std::fs::remove_file(data).unwrap();
    }
}

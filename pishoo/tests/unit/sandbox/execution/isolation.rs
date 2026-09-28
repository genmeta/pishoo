use super::*;

#[tokio::test]
async fn filesystem_grants_are_private_writable_and_hold_the_open_directory() {
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
        .is_ok()
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
        let error = Lib::load(runtime.clone(), id.into(), &[], directory.path())
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
        let error = Lib::load(runtime.clone(), "test".into(), &bytes, &data)
            .err()
            .unwrap();
        assert!(matches!(error, Error::InvalidComponent(_)));
        std::fs::remove_file(data).unwrap();
    }
}

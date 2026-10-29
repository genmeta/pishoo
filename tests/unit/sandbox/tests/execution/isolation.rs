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
    assert!(
        !root.path().join("first/.file").exists(),
        "loading a Lib does not create guest directories"
    );
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
    assert_eq!(dirs[0].1, "/db");
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
    view.create_directory_at(Resource::new_borrow(descriptor), ".file".into())
        .await
        .unwrap();
    assert!(root.path().join("saved/.file").is_dir());
    assert!(!root.path().join("first/.file").exists());
    let created = view
        .open_at(
            Resource::new_borrow(descriptor),
            PathFlags::empty(),
            ".file/created".into(),
            OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
            DescriptorFlags::WRITE,
        )
        .await
        .unwrap();
    view.write(
        Resource::new_borrow(created.rep()),
        b"through preopened handle".to_vec(),
        0,
    )
    .await
    .unwrap();
    HostDescriptor::drop(&mut view, created).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("saved/.file/created")).unwrap(),
        b"through preopened handle"
    );
    assert!(!root.path().join("first/.file/created").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = root.path().join("saved").metadata().unwrap();
        let file = root.path().join("saved/.file/created").metadata().unwrap();
        assert_eq!((file.uid(), file.gid()), (owner.uid(), owner.gid()));
    }
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

#[tokio::test]
#[ignore = "requires root; run in an isolated filesystem"]
async fn root_guest_error_and_cancellation_repair_actual_preopened_directory() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use wasmtime::component::Resource;
    use wasmtime_wasi::{
        filesystem::WasiFilesystemCtxView,
        p2::bindings::filesystem::{
            preopens::Host,
            types::{DescriptorFlags, HostDescriptor, OpenFlags, PathFlags},
        },
    };
    assert!(rustix::process::geteuid().is_root());
    for cancelled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("db");
        let data = database.join("test");
        std::fs::create_dir_all(&data).unwrap();
        rustix::fs::fchown(
            std::fs::File::open(&database).unwrap(),
            Some(rustix::fs::Uid::from_raw(10001)),
            Some(rustix::fs::Gid::from_raw(10002)),
        )
        .unwrap();
        let lib = super::host_client::host_lib(&data);
        let mut filesystem = lib.filesystem.clone();
        let mut table = ResourceTable::new();
        let mut view = WasiFilesystemCtxView {
            ctx: &mut filesystem,
            table: &mut table,
        };
        let directories = Host::get_directories(&mut view).unwrap();
        let file = view
            .open_at(
                Resource::new_borrow(directories[0].0.rep()),
                PathFlags::empty(),
                "created".into(),
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                DescriptorFlags::WRITE,
            )
            .await
            .unwrap();
        view.write(
            Resource::new_borrow(file.rep()),
            b"guest-owned bytes".to_vec(),
            0,
        )
        .await
        .unwrap();
        HostDescriptor::drop(&mut view, file).unwrap();
        assert_eq!(data.join("created").metadata().unwrap().uid(), 0);
        let saved = database.join("saved");
        std::fs::rename(&data, &saved).unwrap();
        std::fs::create_dir(&data).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"external bytes").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o640)).unwrap();
        let original = outside.metadata().unwrap();
        symlink(&outside, saved.join("symlink")).unwrap();
        std::fs::hard_link(&outside, saved.join("hardlink")).unwrap();
        let tasks = TaskTracker::new();
        let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
        let outgoing = if cancelled {
            let access = Arc::new(
                access_control::AccessService::load_from_db(
                    "sqlite::memory:",
                    endpoint.name(),
                    &access_control::SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
                )
                .await
                .unwrap(),
            );
            let api = axum::Router::new().route(
                "/std/native",
                axum::routing::get(|| async { std::future::pending::<&'static str>().await }),
            );
            tokio::time::pause();
            Outgoing::Granted {
                endpoint: endpoint.clone(),
                access,
                api,
            }
        } else {
            Outgoing::Denied
        };
        let result = Invocation::run(lib, endpoint, tasks.clone(), outgoing).await;
        if cancelled {
            assert!(matches!(result, Err(Error::Deadline)));
        } else {
            assert!(result.is_err());
        }
        reaped(&tasks).await;
        let file = saved.join("created").metadata().unwrap();
        assert_eq!((file.uid(), file.gid()), (10001, 10002));
        assert_eq!(
            std::fs::read(saved.join("created")).unwrap(),
            b"guest-owned bytes"
        );
        assert_eq!(
            data.metadata().unwrap().uid(),
            0,
            "replacement path must remain untouched"
        );
        let metadata = outside.metadata().unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid(), metadata.mode()),
            (original.uid(), original.gid(), original.mode())
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"external bytes");
    }
}

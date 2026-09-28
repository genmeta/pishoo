#[tokio::main]
async fn main() {
    if let Err(error) = setup_tcp_demo().await {
        eprintln!("pishoo tcp demo setup: {error}");
        std::process::exit(1);
    }
}

async fn setup_tcp_demo() -> Result<(), Box<dyn std::error::Error>> {
    use rcgen::{CertificateParams, IsCa, KeyIdMethod, KeyPair};

    let home = std::env::var_os("DHTTP_HOME").ok_or("set DHTTP_HOME to an empty directory")?;
    let home = std::path::PathBuf::from(home);
    std::fs::create_dir_all(&home)?;
    if std::fs::read_dir(&home)?.next().is_some() {
        return Err("DHTTP_HOME must be empty for the TCP demo setup".into());
    }
    let proxy_port = std::env::var("PISHOO_PROXY_PORT")
        .unwrap_or_else(|_| "18474".to_owned())
        .parse::<u16>()?;
    let name = "demo.dhttp.net";
    let hash = "a".repeat(64);
    let root = home.join("demo");
    std::fs::create_dir_all(root.join("ssl"))?;
    std::fs::create_dir_all(root.join("db"))?;
    std::fs::create_dir_all(root.join("file"))?;
    let mut params = CertificateParams::new(vec![name.to_owned()])?;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_identifier_method = KeyIdMethod::PreSpecified(format!("0:0:{hash}").into_bytes());
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    std::fs::write(root.join("ssl/fullchain.crt"), cert.pem())?;
    std::fs::write(root.join("ssl/privkey.pem"), key.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("ssl/privkey.pem"),
            std::fs::Permissions::from_mode(0o400),
        )?;
    }
    std::fs::write(root.join("ssl/ocsp.der"), [1_u8])?;

    let db = rusqlite::Connection::open(root.join("db/config.db"))?;
    db.execute_batch(
        "PRAGMA user_version=1;
             CREATE TABLE settings(listen INTEGER NOT NULL, exec INTEGER NOT NULL);
             INSERT INTO settings VALUES(1,0);
             CREATE TABLE proxy_locations(location TEXT NOT NULL, proxy_pass TEXT NOT NULL);",
    )?;
    db.execute(
        "INSERT INTO proxy_locations VALUES(?1,?2)",
        ("/proxy/", format!("http://127.0.0.1:{proxy_port}/remote/")),
    )?;
    db.execute(
        "INSERT INTO proxy_locations VALUES(?1,?2)",
        (
            "= /exact",
            format!("http://127.0.0.1:{proxy_port}/remote/hello.txt"),
        ),
    )?;
    db.execute(
        "INSERT INTO proxy_locations VALUES(?1,?2)",
        ("= /duplex", format!("http://127.0.0.1:{proxy_port}/duplex")),
    )?;
    std::fs::write(root.join("file/index.html"), "<h1>Pishoo TCP demo</h1>\n")?;
    std::fs::write(root.join("file/hello.txt"), "static from demo\n")?;
    std::fs::create_dir_all(root.join("file/remote"))?;
    std::fs::write(root.join("file/remote/hello.txt"), "static from upstream\n")?;
    for (id, bytes) in [
        (
            "echo",
            include_bytes!("../../assets/tcp-demo-echo.wasm").as_slice(),
        ),
        (
            "info",
            include_bytes!("../../assets/tcp-demo-info.wasm").as_slice(),
        ),
        (
            "trailers",
            include_bytes!("../../assets/tcp-demo-trailers.wasm").as_slice(),
        ),
    ] {
        let lib_dir = root.join("lib").join(id);
        std::fs::create_dir_all(&lib_dir)?;
        std::fs::write(lib_dir.join("lib.wasm"), bytes)?;
    }
    let uri = format!("sqlite://{}?mode=rwc", root.join("db/access.db").display());
    let subject =
        access_control::SubjectId::new(hash.into_bytes()).map_err(|_| "invalid demo subject")?;
    let access = access_control::AccessService::load_from_db(&uri, name, &subject).await?;
    for api in ["/file/hello.txt", "/proxy/hello.txt", "/exact"] {
        access
            .set_policy(
                access_control::Method::Specified(http::Method::GET),
                api,
                access_control::Effect::Allow,
                access_control::Grantee::Anony,
            )
            .await?;
    }
    access
        .set_policy(
            access_control::Method::Specified(http::Method::POST),
            "/duplex",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await?;
    for entry in std::fs::read_dir(root.join("lib"))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid demo Lib directory name")?;
        let bytes = std::fs::read(entry.path().join("lib.wasm"))?;
        let openapi = pishoo::validate_lib(&bytes)?;
        for (path, item) in openapi.paths.ok_or("validated Lib has no paths")? {
            let api = format!("/api/{id}{path}");
            for (method, _) in item.methods() {
                access
                    .set_policy(
                        access_control::Method::Specified(method),
                        &api,
                        access_control::Effect::Allow,
                        access_control::Grantee::Anony,
                    )
                    .await?;
            }
        }
    }
    eprintln!("TCP demo home: {}", home.display());
    Ok(())
}

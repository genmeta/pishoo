//! Import into a private staging profile, then durably publish a managed identity.
use std::{io::Write, path::Path};

use dhttp_home::{DhttpHome, identity::IdentityProfile};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::storage::{canonical, conflict, private_directory};
use crate::{Error, Result};

pub(super) fn profile(home: &DhttpHome, name: &str) -> IdentityProfile {
    IdentityProfile::try_from(home.join(name.trim_end_matches(dhttp_home::DHTTP_SUFFIX))).unwrap()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn verify_chain(
    certs: &[qtls::CertificateDer<'_>],
    name: &str,
    root_der: &[u8],
) -> Result<Vec<u8>> {
    let bad = || Error::BadRequest("invalid identity certificate chain".into());
    let first = certs.first().ok_or_else(bad)?;
    if certs.len() > 8 {
        return Err(bad());
    }
    let root = qtls::CertificateDer::from(root_der);
    let anchor = webpki::anchor_from_trusted_cert(&root).map_err(|_| bad())?;
    let leaf = webpki::EndEntityCert::try_from(first).map_err(|_| bad())?;
    leaf.verify_for_usage(
        webpki::ALL_VERIFICATION_ALGS,
        &[anchor],
        &certs[1..],
        qtls::UnixTime::now(),
        webpki::KeyUsage::server_auth(),
        None,
        None,
    )
    .map_err(|_| bad())?;
    leaf.verify_is_valid_for_subject_name(&qtls::ServerName::try_from(name).map_err(|_| bad())?)
        .map_err(|_| bad())?;
    let ski =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(certs).map_err(|_| bad())?;
    if ski.chain().usage() != dhttp_home::certificate::CertificateUsage::ClientAndServer {
        return Err(bad());
    }
    Ok(ski.owner_hash().as_str().as_bytes().to_vec())
}

pub(super) async fn create(
    home: &DhttpHome,
    db: &mut Connection,
    body: Value,
    root_der: &[u8],
) -> Result<Value> {
    let object = body
        .as_object()
        .ok_or_else(|| Error::BadRequest("user requires a JSON object".into()))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "name" | "certificate" | "private_key" | "ocsp"
        )
    }) {
        return Err(Error::BadRequest("unknown user field".into()));
    }
    let name = canonical(super::required(&body, "name")?)?;
    let certificate = super::required(&body, "certificate")?;
    let key = super::required(&body, "private_key")?;
    let staging = tempfile::Builder::new()
        .prefix("create-")
        .tempdir_in(home.join(".system"))?;
    let staged = profile(&DhttpHome::new(staging.path().into()), &name);
    private_directory(&staged.ssl_dir())?;
    write_private(&staged.cert_path(), certificate.as_bytes())?;
    write_private(&staged.key_path(), key.as_bytes())?;
    let certs = staged
        .load_certs()
        .await
        .map_err(|_| Error::BadRequest("invalid certificate PEM".into()))?;
    let subject = verify_chain(&certs, &name, root_der)?;
    let key = staged
        .load_key()
        .await
        .map_err(|_| Error::BadRequest("invalid private key PEM".into()))?;
    let signing_key = qtls::default_provider()
        .key_provider
        .load_private_key(key)
        .map_err(|_| Error::BadRequest("unsupported private key".into()))?;
    let (_, leaf) = x509_parser::parse_x509_certificate(certs[0].as_ref())
        .map_err(|_| Error::BadRequest("invalid leaf certificate".into()))?;
    if signing_key
        .public_key()
        .is_none_or(|public_key| public_key.as_ref() != leaf.public_key().raw)
    {
        return Err(Error::BadRequest(
            "private key does not match certificate".into(),
        ));
    }
    let previous: Option<(Vec<u8>, String)> = db
        .query_row(
            "SELECT subject_id,phase FROM managed_user WHERE name=?1",
            [&name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((owner, phase)) = previous {
        if owner != subject || phase != "active" {
            return Err(conflict("user_creation_in_progress_or_owner_changed"));
        }
        check_profile(profile(home, &name).path(), &subject)?;
        return Ok(json!({"name":name,"created":false}));
    }
    match profile(home, &name).path().symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(conflict("identity_path_already_exists")),
    }
    let registered: Option<Vec<u8>> = db
        .query_row(
            "SELECT subject_id FROM administrator WHERE name=?1",
            [&name],
            |row| row.get(0),
        )
        .optional()?;
    if registered.as_deref().is_some_and(|owner| owner != subject) {
        return Err(conflict("administrator_owner_changed"));
    }
    if crate::server::local_profiles(home)?
        .iter()
        .any(|p| p.name() == name)
    {
        return Err(conflict("identity_already_exists"));
    }
    let ocsp = match body.get("ocsp") {
        Some(value) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(
                    value
                        .as_str()
                        .ok_or_else(|| Error::BadRequest("ocsp must be base64".into()))?,
                )
                .map_err(|_| Error::BadRequest("ocsp must be base64".into()))?
        }
        None => crate::server::ocsp::fetch_ocsp(&certs).await?,
    };
    qtls::validate_ocsp(&ocsp, &certs, qtls::UnixTime::now())
        .map_err(|_| Error::BadRequest("invalid OCSP response".into()))?;
    write_private(&staged.ocsp_path(), &ocsp)?;
    crate::configure(&staged, Some("all"), true)?;
    let pending = home.join(".system/pending");
    private_directory(&pending)?;
    let pending_profile = profile(&DhttpHome::new(pending.clone()), &name);
    if pending_profile.path().try_exists()? {
        // A crash before the journal commit can leave an unpublished directory.
        check_profile(pending_profile.path(), &subject)?;
        std::fs::remove_dir_all(pending_profile.path())?;
    }
    sync_tree(staged.path())?;
    std::fs::rename(staged.path(), pending_profile.path())?;
    let pending_profile = scopeguard::guard(pending_profile, |pending| {
        let _ = std::fs::remove_dir_all(pending.path());
    });
    sync_directory(&pending)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO managed_user(name,subject_id,profile_path,phase) VALUES(?1,?2,?3,'pending')",
        params![name, subject, profile(home, &name).path().to_string_lossy()],
    )?;
    tx.commit()?;
    scopeguard::ScopeGuard::into_inner(pending_profile);
    recover_user(
        home,
        db,
        &name,
        "pending",
        &subject,
        profile(home, &name).path(),
    )?;
    Ok(json!({"name":name,"created":true}))
}

fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_tree(path: &Path) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            std::fs::File::open(entry.path())?.sync_all()?;
        }
    }
    sync_directory(path)
}

fn check_profile(path: &Path, subject: &[u8]) -> Result<()> {
    if !path.symlink_metadata()?.file_type().is_dir() {
        return Err(conflict("managed_identity_path_changed"));
    }
    let pem = std::fs::read(path.join("ssl/fullchain.crt"))?;
    let certificates = x509_parser::pem::Pem::iter_from_buffer(&pem)
        .map(|item| item.map(|pem| qtls::CertificateDer::from(pem.contents)))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| conflict("managed_identity_owner_changed"))?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certificates)
        .map_err(|_| conflict("managed_identity_owner_changed"))?;
    if ski.owner_hash().as_str().as_bytes() != subject {
        return Err(conflict("managed_identity_owner_changed"));
    }
    Ok(())
}

/// One damaged identity must not prevent healthy identities from serving.
pub(super) fn recover(home: &DhttpHome, db: &mut Connection) -> Result<()> {
    let rows = db.prepare("SELECT name,phase,subject_id,profile_path FROM managed_user WHERE phase<>'active' ORDER BY name")?
        .query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Vec<u8>>(2)?,row.get::<_,String>(3)?)))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    for (name, phase, subject, path) in rows {
        if let Err(error) = recover_user(home, db, &name, &phase, &subject, Path::new(&path)) {
            tracing::warn!(identity=%name, %error, "Identity recovery failed; retaining journal for retry");
        }
    }
    Ok(())
}

fn trash_directory(parent: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mut directory = std::fs::File::from(
        rustix::fs::open(parent, flags, rustix::fs::Mode::empty()).map_err(std::io::Error::from)?,
    );
    let owner = directory.metadata()?;
    for part in [".system", "trash"] {
        match rustix::fs::mkdirat(&directory, part, rustix::fs::Mode::from_raw_mode(0o700)) {
            Ok(()) => directory.sync_all()?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        directory = std::fs::File::from(
            rustix::fs::openat(&directory, part, flags, rustix::fs::Mode::empty())
                .map_err(std::io::Error::from)?,
        );
        if rustix::process::geteuid().is_root() {
            rustix::fs::fchown(
                &directory,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(std::io::Error::from)?;
        }
    }
    Ok(directory)
}

fn recover_user(
    home: &DhttpHome,
    db: &mut Connection,
    name: &str,
    phase: &str,
    subject: &[u8],
    path: &Path,
) -> Result<()> {
    let destination = IdentityProfile::try_from(path)
        .map_err(|_| Error::InvalidConfig("invalid managed identity path".into()))?;
    if !path.is_absolute() || destination.name() != name {
        return Err(Error::InvalidConfig(
            "managed identity path does not match name".into(),
        ));
    }
    if phase == "pending" {
        let source = profile(&DhttpHome::new(home.join(".system/pending")), name);
        if source.path().try_exists()? {
            check_profile(source.path(), subject)?;
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                source.path(),
                rustix::fs::CWD,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(std::io::Error::from)?;
            sync_directory(path.parent().unwrap())?;
            sync_directory(&home.join(".system/pending"))?;
        }
        check_profile(path, subject)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE managed_user SET phase='active' WHERE name=?1",
            [name],
        )?;
        tx.commit()?;
        return Ok(());
    }
    let parent = path.parent().unwrap();
    let trash = trash_directory(parent)?;
    let removed = profile(&DhttpHome::new(parent.join(".system/trash")), name);
    if phase == "deleting" {
        if !removed.path().try_exists()? && path.try_exists()? {
            check_profile(path, subject)?;
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                path,
                &trash,
                removed.path().file_name().unwrap(),
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(std::io::Error::from)?;
            sync_directory(parent)?;
            trash.sync_all()?;
        }
        if removed.path().try_exists()? {
            check_profile(removed.path(), subject)?;
        }
        db.execute(
            "UPDATE managed_user SET phase='removed' WHERE name=?1",
            [name],
        )?;
    }
    let directory = cap_std::fs::Dir::from_std_file(trash);
    match directory.remove_dir_all(removed.path().file_name().unwrap()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    directory.into_std_file().sync_all()?;
    db.execute("DELETE FROM managed_user WHERE name=?1", [name])?;
    Ok(())
}

pub(super) async fn delete(home: &DhttpHome, db: &mut Connection, name: &str) -> Result<Value> {
    let name = canonical(name)?;
    let recorded = db
        .query_row(
            "SELECT subject_id,profile_path FROM managed_user WHERE name=?1",
            [&name],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let (subject, path) = if let Some(recorded) = recorded {
        let phase: String = db.query_row(
            "SELECT phase FROM managed_user WHERE name=?1",
            [&name],
            |row| row.get(0),
        )?;
        if phase == "pending" {
            recover_user(home, db, &name, &phase, &recorded.0, Path::new(&recorded.1))?;
        }
        recorded
    } else {
        let Some(profile) = crate::server::local_profiles(home)?
            .into_iter()
            .find(|profile| profile.name() == name)
        else {
            return Ok(json!({"name":name,"deleted":false}));
        };
        let certs = profile
            .load_certs()
            .await
            .map_err(|_| Error::InvalidIdentity("cannot inspect identity certificate".into()))?;
        let visitor = super::identity(profile.name(), &certs)?;
        (
            visitor.subject_id().as_bytes().to_vec(),
            profile.path().to_string_lossy().into_owned(),
        )
    };
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO managed_user(name,subject_id,profile_path,phase) VALUES(?1,?2,?3,'deleting') ON CONFLICT(name) DO UPDATE SET phase='deleting' WHERE managed_user.phase='active'",
        params![name, subject, path],
    )?;
    tx.commit()?;
    let phase: String = db.query_row(
        "SELECT phase FROM managed_user WHERE name=?1",
        [&name],
        |row| row.get(0),
    )?;
    recover_user(home, db, &name, &phase, &subject, Path::new(&path))?;
    Ok(json!({"name":name,"deleted":true}))
}

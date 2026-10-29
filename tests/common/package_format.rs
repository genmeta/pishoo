use super::*;

pub(crate) fn waz_write(files: &std::collections::BTreeMap<String, Vec<u8>>) -> WazResult<Vec<u8>> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6))
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for (name, bytes) in files {
        writer.start_file(name, options)?;
        writer.write_all(bytes)?;
    }
    let bytes = writer.finish()?.into_inner();
    waz_read(&bytes)?;
    Ok(bytes)
}

// Runtime fixtures extract the same raw component as installed packages.
pub(crate) fn runtime_fixture(bytes: &[u8]) -> Vec<u8> {
    waz_read(bytes).unwrap().remove("backend.wasm").unwrap()
}

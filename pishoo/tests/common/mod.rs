use pishoo::{sandbox::Sandbox, wasm::Error};

pub fn component(bytes: &[u8], apis: &[(&str, &str)]) -> Vec<u8> {
    fn leb(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let byte = (n & 127) as u8;
            n >>= 7;
            out.push(byte | if n != 0 { 128 } else { 0 });
            if n == 0 {
                break;
            }
        }
    }
    let mut paths = serde_json::Map::new();
    for (method, path) in apis {
        let item = paths
            .entry(path.to_string())
            .or_insert_with(|| serde_json::json!({}));
        item.as_object_mut()
            .unwrap()
            .insert(method.to_ascii_lowercase(), serde_json::json!({}));
    }
    let doc = serde_json::to_vec(&serde_json::json!({"openapi":"3.1.0", "info":{"title":"Test","version":"1"}, "paths":paths})).unwrap();
    let mut section = Vec::new();
    leb(b"pishoo:openapi".len(), &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend(doc);
    let mut bytes = bytes.to_vec();
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    bytes
}

pub fn load_lib(sandbox: &Sandbox, id: &str, bytes: &[u8]) -> Result<(), Error> {
    sandbox.load_lib(
        id,
        &component(
            bytes,
            &[
                ("POST", "/cancel"),
                ("POST", "/pending"),
                ("POST", "/read"),
                ("POST", "/early"),
                ("POST", "/absent/small/normal"),
            ],
        ),
    )
}

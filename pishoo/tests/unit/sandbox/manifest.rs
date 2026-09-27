use super::*;

fn component(document: &str) -> Vec<u8> {
    fn leb(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let b = (n & 127) as u8;
            n >>= 7;
            out.push(b | if n > 0 { 128 } else { 0 });
            if n == 0 {
                break;
            }
        }
    }
    let mut section = Vec::new();
    leb(14, &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(document.as_bytes());
    let mut wasm = b"\0asm\x0d\0\x01\0".to_vec();
    wasm.push(0);
    leb(section.len(), &mut wasm);
    wasm.extend(section);
    wasm
}
#[test]
fn manifest_rejects_duplicates_templates_references_and_reserved_routes() {
    let document = r#"{"openapi":"3.1.0","info":{"title":"test","version":"1"},"paths":{"/echo":{"get":{"responses":{"200":{"description":"ok"}}}}}}"#;
    let valid = component(document);
    assert!(validate_lib(&valid).is_ok());
    for broken in [
        document.replace("\"title\":\"test\"", "\"title\":\"a\",\"title\":\"b\""),
        document.replace("/echo", "/workspace"),
        document.replace("/echo", "/{id}"),
        document.replace(
            "\"get\":",
            "\"$ref\":\"https://example.com/schema\",\"get\":",
        ),
    ] {
        assert!(validate_lib(&component(&broken)).is_err());
    }
    let mut duplicate = valid.clone();
    duplicate.extend_from_slice(&valid[8..]);
    assert!(validate_lib(&duplicate).is_err());
}

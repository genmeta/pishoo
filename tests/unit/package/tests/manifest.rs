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
fn manifest_rejects_templates_and_path_references() {
    let document = r#"{"openapi":"3.1.0","info":{"title":"test","version":"1"},"paths":{"/echo":{"get":{"responses":{"200":{"description":"ok"}}}}}}"#;
    let valid = component(document);
    assert!(validate_lib(&valid).is_ok());
    for broken in [
        document.replace("/echo", "/{id}"),
        document.replace(
            "\"get\":",
            "\"$ref\":\"#/components/pathItems/echo\",\"get\":",
        ),
    ] {
        assert!(validate_lib(&component(&broken)).is_err());
    }
    for path in [
        "/workspace",
        "/api",
        "/std-extra",
        "/std/xxxx",
        "/contact",
        "/file/a",
        "/acl",
    ] {
        assert!(
            validate_lib(&component(&document.replace("/echo", path))).is_ok(),
            "{path}"
        );
    }
    let mut duplicate = valid.clone();
    duplicate.extend_from_slice(&valid[8..]);
    assert!(validate_lib(&duplicate).is_err());
}

#[test]
fn manifest_uses_oas3_deserialization_for_duplicate_keys_and_external_refs() {
    let duplicate_path = r#"{"openapi":"3.1.0","info":{"title":"test","version":"1"},"paths":{"/echo":{"post":{}},"/echo":{"get":{}}}}"#;
    let openapi = validate_lib(&component(duplicate_path)).unwrap();
    assert!(openapi.paths.unwrap().get("/echo").unwrap().get.is_some());

    let external_response = r#"{"openapi":"3.1.0","info":{"title":"test","version":"1"},"paths":{"/echo":{"get":{"responses":{"200":{"$ref":"https://example.com/response"}}}}}}"#;
    assert!(validate_lib(&component(external_response)).is_ok());

    assert!(validate_lib(&component("{ invalid JSON }")).is_err());
}

#[test]
fn independent_openapi_uses_the_same_rules_as_component_documents() {
    let document = r#"{"openapi":"3.1.0","info":{"title":"test","version":"1"},"paths":{"/echo":{"get":{"operationId":"echo","responses":{"200":{"description":"ok"}}}}}}"#;
    assert!(crate::validate_openapi(document.as_bytes()).is_ok());
    assert_eq!(
        serde_json::to_value(crate::validate_openapi(document.as_bytes()).unwrap()).unwrap(),
        serde_json::to_value(validate_lib(&component(document)).unwrap()).unwrap()
    );
    for broken in [document.replace("3.1.0", "3.0.0"), "invalid JSON".into()] {
        assert!(crate::validate_openapi(broken.as_bytes()).is_err());
        assert!(validate_lib(&component(&broken)).is_err());
    }
    assert!(crate::validate_openapi(&vec![b' '; 1024 * 1024 + 1]).is_err());
    assert!(validate_lib(b"\0asm\x0d\0\x01\0").is_err());
}

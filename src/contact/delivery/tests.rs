use super::*;

#[test]
fn approval_wire_formats_are_distinct_from_queued_contact_receipts() {
    assert_eq!(
        pending_review(202, &json!({"status":"pending_review","review_id":12})).unwrap(),
        Some(12)
    );
    assert_eq!(
        pending_review(400, &json!({"error":"need_review","review_id":12})).unwrap(),
        Some(12)
    );
    assert_eq!(
        pending_review(202, &json!({"status":"pending","delivery_status":"queued"})).unwrap(),
        None
    );
    assert!(pending_review(202, &json!({"status":"pending_review"})).is_err());
    assert!(pending_review(202, &json!({"status":"pending_review","review_id":0})).is_err());
    assert_eq!(
        review_progress(202, &json!({"status":"pending_review","review_id":12}))
            .unwrap_err()
            .0,
        409
    );
}

#[test]
fn status_polling_never_creates_another_approval_or_accepts_a_relationship() {
    assert!(!review_progress(200, &json!({"status":"pending"})).unwrap());
    for status in ["allowed", "consumed"] {
        assert!(review_progress(200, &json!({"status":status})).unwrap());
    }
    assert_eq!(
        review_progress(200, &json!({"status":"denied"})).unwrap_err(),
        fail(409, "review_denied")
    );
    assert_eq!(
        review_progress(200, &json!({"status":"expired"})).unwrap_err(),
        fail(410, "review_expired")
    );
    assert_eq!(
        review_progress(400, &json!({"error":"need_review","review_id":99}))
            .unwrap_err()
            .0,
        409
    );
}

#[test]
fn legacy_capabilities_are_never_reinterpreted() {
    assert!(operation_snapshot(None, r#"["Receive messages"]"#).is_err());
    assert_eq!(operation_snapshot(None, "[]").unwrap(), json!({}));
    let snapshot = r#"{"/std/message":["POST"]}"#;
    assert_eq!(
        operation_snapshot(Some(snapshot), r#"["Receive messages"]"#).unwrap(),
        json!({"/std/message":["POST"]})
    );
}

#[test]
fn fixed_wire_values_and_fragmented_open_are_unambiguous() {
    let frame = Frame::Open {
        rows: 24,
        cols: 80,
        term: "xterm".into(),
        cwd: "work".into(),
    };
    let wire = encoded(&[frame]);
    assert_eq!(
        &wire[..],
        &[
            1, 0, 0, 0, 16, 0, 24, 0, 80, 5, b'x', b't', b'e', b'r', b'm', 0, 4, b'w', b'o', b'r',
            b'k'
        ]
    );
    for split in 0..wire.len() {
        let mut buffer = BytesMut::from(&wire[..split]);
        assert!(Frame::decode(&mut buffer).unwrap().is_none());
        assert_eq!(buffer.as_ref(), &wire[..split]);
        buffer.extend_from_slice(&wire[split..]);
        assert_eq!(
            Frame::decode(&mut buffer).unwrap(),
            Some(Frame::Open {
                rows: 24,
                cols: 80,
                term: "xterm".into(),
                cwd: "work".into()
            })
        );
        assert!(buffer.is_empty());
    }
    assert_eq!(
        encoded(&[Frame::Resize {
            rows: 1,
            cols: 1000
        }])
        .as_ref(),
        &[3, 0, 0, 0, 4, 0, 1, 3, 232]
    );
    assert_eq!(
        encoded(&[Frame::Exit { kind: 2, value: 6 }]).as_ref(),
        &[0x12, 0, 0, 0, 5, 2, 0, 0, 0, 6]
    );
}

#[test]
fn arbitrary_data_and_each_control_frame_round_trip() {
    let frames = [
        Frame::Input(Bytes::from_static(&[0, 255, 128])),
        Frame::Resize {
            rows: 1000,
            cols: 1,
        },
        Frame::InputEnd,
        Frame::Signal(3),
        Frame::Cancel,
        Frame::Ready {
            session_id: u64::MAX,
            mode: 1,
        },
        Frame::Output(Bytes::from_static(&[255, 0])),
        Frame::Exit {
            kind: 0,
            value: 255,
        },
        Frame::Error {
            code: 1,
            message: "bad input".into(),
        },
    ];
    let mut bytes = BytesMut::from(encoded(&frames).as_ref());
    for frame in frames {
        assert_eq!(Frame::decode(&mut bytes).unwrap(), Some(frame));
    }
    assert!(bytes.is_empty());
}

#[test]
fn oversize_unknown_and_invalid_control_values_fail_before_consumption() {
    for wire in [
        &[2, 0, 0, 64, 1][..],
        &[0xff, 0, 0, 0, 0],
        &[4, 0, 0, 0, 1],
        &[3, 0, 0, 0, 4, 0, 0, 0, 1],
        &[5, 0, 0, 0, 1, 4],
        &[0x10, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 1, 2],
        &[0x12, 0, 0, 0, 5, 0, 0, 0, 1, 0],
        &[0x12, 0, 0, 0, 5, 2, 0, 0, 0, 7],
        &[0x13, 0, 0, 0, 3, 0, 1, 255],
    ] {
        let mut buffer = BytesMut::from(wire);
        assert!(Frame::decode(&mut buffer).is_err(), "{wire:?}");
        assert_eq!(buffer.as_ref(), wire);
    }
    let mut output = BytesMut::from(&b"prior"[..]);
    assert!(Frame::Signal(0).encode(&mut output).is_err());
    assert_eq!(output.as_ref(), b"prior");
}

#[test]
fn open_rejects_ambiguous_and_reserved_paths_and_unsupported_terms() {
    for cwd in [
        "/etc",
        "..",
        "a/../b",
        "a//b",
        "a/./b",
        "a/",
        "a\\b",
        "%2e%2e",
        "C:work",
        "\0",
        ".ssh/key",
        ".config",
        "Library/LaunchAgents",
    ] {
        assert!(validate_open(24, 80, "xterm", cwd).is_err(), "{cwd:?}");
    }
    assert!(validate_open(24, 80, "screen", "").is_err());
    assert!(validate_open(24, 80, "xterm", &"a".repeat(1025)).is_err());
    assert!(validate_open(24, 80, "xterm", "notes/会议").is_ok());
}

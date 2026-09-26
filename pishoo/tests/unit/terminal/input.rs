#[tokio::test]
async fn input_end_preserves_controls_until_http_eof() {
    let mut input = TerminalInput::Data(body(encoded(&[
        Frame::InputEnd,
        Frame::Resize {
            rows: 30,
            cols: 100,
        },
        Frame::Signal(1),
        Frame::Cancel,
    ])));
    let mut buffer = BytesMut::new();
    assert_eq!(
        input.read_next(&mut buffer).await.unwrap(),
        Some(Frame::InputEnd)
    );
    assert!(matches!(input, TerminalInput::Controls(_)));
    assert_eq!(
        input.read_next(&mut buffer).await.unwrap(),
        Some(Frame::Resize {
            rows: 30,
            cols: 100
        })
    );
    assert_eq!(
        input.read_next(&mut buffer).await.unwrap(),
        Some(Frame::Signal(1))
    );
    assert_eq!(
        input.read_next(&mut buffer).await.unwrap(),
        Some(Frame::Cancel)
    );
    assert!(input.read_next(&mut buffer).await.unwrap().is_none());
    assert!(matches!(input, TerminalInput::Ended));
    assert!(input.read_next(&mut buffer).await.unwrap().is_none());
}

#[tokio::test]
async fn invalid_direction_repeated_end_and_truncation_release_input() {
    for bytes in [
        encoded(&[Frame::InputEnd, Frame::Input(Bytes::from_static(b"late"))]),
        encoded(&[Frame::InputEnd, Frame::InputEnd]),
        encoded(&[Frame::Output(Bytes::new())]),
        Bytes::from_static(&[1, 0, 0]),
    ] {
        let mut input = TerminalInput::Data(body(bytes));
        let mut buffer = BytesMut::new();
        loop {
            match input.read_next(&mut buffer).await {
                Err(_) => break,
                Ok(Some(_)) => {}
                Ok(None) => panic!("malformed input must fail"),
            }
        }
        assert!(matches!(input, TerminalInput::Ended));
        assert!(buffer.is_empty());
        assert!(input.read_next(&mut buffer).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn body_failure_is_not_an_eof() {
    let frames = futures::stream::iter([Err::<http_body::Frame<Bytes>, dhttp::BoxError>(
        Box::new(std::io::Error::other("transport reset")),
    )]);
    let mut input = TerminalInput::Data(StreamBody::new(frames).boxed_unsync());
    assert!(input.read_next(&mut BytesMut::new()).await.is_err());
    assert!(matches!(input, TerminalInput::Ended));
}

#[allow(dead_code)] // Used by session.run when the execution backend is implemented.
enum TerminalInput {
    Data(dhttp::Body),
    Controls(dhttp::Body),
    Ended,
}

impl TerminalInput {
    #[allow(dead_code)]
    async fn read_next(&mut self, buffer: &mut BytesMut) -> Result<Option<Frame>> {
        let result = async {
            loop {
                if matches!(self, Self::Ended) {
                    return Ok(None);
                }
                if let Some(frame) = Frame::decode(buffer)? {
                    if matches!(
                        frame,
                        Frame::Ready { .. }
                            | Frame::Output(_)
                            | Frame::Exit { .. }
                            | Frame::Error { .. }
                    ) {
                        return Err(invalid("terminal frame has the wrong direction"));
                    }
                    if matches!(self, Self::Controls(_))
                        && !matches!(
                            frame,
                            Frame::Resize { .. } | Frame::Signal(_) | Frame::Cancel
                        )
                    {
                        return Err(invalid("terminal input already ended"));
                    }
                    if matches!(frame, Frame::InputEnd) {
                        let Self::Data(body) = std::mem::replace(self, Self::Ended) else {
                            unreachable!("only Data accepts INPUT_END")
                        };
                        *self = Self::Controls(body);
                    }
                    return Ok(Some(frame));
                }
                let body = match self {
                    Self::Data(body) | Self::Controls(body) => body,
                    Self::Ended => return Ok(None),
                };
                match body.frame().await {
                    Some(Ok(frame)) => {
                        let data = frame
                            .into_data()
                            .map_err(|_| invalid("terminal request trailers are unsupported"))?;
                        if data.len() > MAX_BUFFER.saturating_sub(buffer.len()) {
                            return Err(invalid("terminal input buffer limit exceeded"));
                        }
                        buffer.extend_from_slice(&data);
                    }
                    Some(Err(error)) => return Err(invalid_body(error)),
                    None => {
                        *self = Self::Ended;
                        if !buffer.is_empty() {
                            return Err(invalid("terminal frame was truncated by EOF"));
                        }
                        return Ok(None);
                    }
                }
            }
        }
        .await;
        if result.is_err() {
            *self = Self::Ended;
            buffer.clear();
        }
        result
    }
}

fn invalid_body(error: dhttp::BoxError) -> Error {
    std::io::Error::other(error).into()
}

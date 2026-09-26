#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    Open {
        rows: u16,
        cols: u16,
        term: String,
        cwd: String,
    },
    Input(Bytes),
    Resize {
        rows: u16,
        cols: u16,
    },
    InputEnd,
    Signal(u8),
    Cancel,
    Ready {
        session_id: u64,
        mode: u8,
    },
    Output(Bytes),
    Exit {
        kind: u8,
        value: u32,
    },
    Error {
        code: u16,
        message: String,
    },
}

impl Frame {
    #[allow(dead_code)]
    fn encode(&self, output: &mut BytesMut) -> Result<()> {
        let mut payload = BytesMut::new();
        let kind = match self {
            Self::Open {
                rows,
                cols,
                term,
                cwd,
            } => {
                validate_open(*rows, *cols, term, cwd)?;
                payload.put_u16(*rows);
                payload.put_u16(*cols);
                payload.put_u8(term.len() as u8);
                payload.extend_from_slice(term.as_bytes());
                payload.put_u16(cwd.len() as u16);
                payload.extend_from_slice(cwd.as_bytes());
                0x01
            }
            Self::Input(data) | Self::Output(data) => {
                if data.len() > MAX_PAYLOAD {
                    return Err(invalid("terminal frame is too large"));
                }
                payload.extend_from_slice(data);
                if matches!(self, Self::Input(_)) {
                    0x02
                } else {
                    0x11
                }
            }
            Self::Resize { rows, cols } => {
                validate_size(*rows, *cols)?;
                payload.put_u16(*rows);
                payload.put_u16(*cols);
                0x03
            }
            Self::InputEnd => 0x04,
            Self::Signal(signal) => {
                if !(1..=3).contains(signal) {
                    return Err(invalid("invalid terminal signal"));
                }
                payload.put_u8(*signal);
                0x05
            }
            Self::Cancel => 0x06,
            Self::Ready { session_id, mode } => {
                if *mode > 1 {
                    return Err(invalid("invalid terminal mode"));
                }
                payload.put_u64(*session_id);
                payload.put_u8(*mode);
                0x10
            }
            Self::Exit { kind, value } => {
                validate_exit(*kind, *value)?;
                payload.put_u8(*kind);
                payload.put_u32(*value);
                0x12
            }
            Self::Error { code, message } => {
                if !(1..=4).contains(code) || message.len() > 1024 {
                    return Err(invalid("invalid terminal error frame"));
                }
                payload.put_u16(*code);
                payload.extend_from_slice(message.as_bytes());
                0x13
            }
        };
        output.reserve(5 + payload.len());
        output.put_u8(kind);
        output.put_u32(payload.len() as u32);
        output.extend_from_slice(&payload);
        Ok(())
    }

    #[allow(dead_code)]
    fn decode(input: &mut BytesMut) -> Result<Option<Self>> {
        if input.len() < 5 {
            return Ok(None);
        }
        let kind = input[0];
        let length = u32::from_be_bytes(input[1..5].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD {
            return Err(invalid("terminal frame is too large"));
        }
        let valid_length = match kind {
            0x01 => (7..=1063).contains(&length),
            0x02 | 0x11 => true,
            0x03 => length == 4,
            0x04 | 0x06 => length == 0,
            0x05 => length == 1,
            0x10 => length == 9,
            0x12 => length == 5,
            0x13 => (2..=1026).contains(&length),
            _ => return Err(invalid("unknown terminal frame type")),
        };
        if !valid_length {
            return Err(invalid("invalid terminal payload length"));
        }
        if input.len() < 5 + length {
            return Ok(None);
        }
        let payload = &input[5..5 + length];
        let frame = match kind {
            0x01 => {
                let rows = u16::from_be_bytes(payload[..2].try_into().unwrap());
                let cols = u16::from_be_bytes(payload[2..4].try_into().unwrap());
                let term_length = payload[4] as usize;
                if term_length > 32 || payload.len() < 7 + term_length {
                    return Err(invalid("invalid terminal OPEN strings"));
                }
                let term = std::str::from_utf8(&payload[5..5 + term_length])
                    .map_err(|_| invalid("invalid terminal TERM encoding"))?;
                let cwd_length = u16::from_be_bytes(
                    payload[5 + term_length..7 + term_length]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if payload.len() != 7 + term_length + cwd_length {
                    return Err(invalid("invalid terminal OPEN strings"));
                }
                let cwd = std::str::from_utf8(&payload[7 + term_length..])
                    .map_err(|_| invalid("invalid terminal cwd encoding"))?;
                validate_open(rows, cols, term, cwd)?;
                Self::Open {
                    rows,
                    cols,
                    term: term.into(),
                    cwd: cwd.into(),
                }
            }
            0x02 => Self::Input(Bytes::copy_from_slice(payload)),
            0x03 => {
                let rows = u16::from_be_bytes(payload[..2].try_into().unwrap());
                let cols = u16::from_be_bytes(payload[2..].try_into().unwrap());
                validate_size(rows, cols)?;
                Self::Resize { rows, cols }
            }
            0x04 => Self::InputEnd,
            0x05 => {
                if !(1..=3).contains(&payload[0]) {
                    return Err(invalid("invalid terminal signal"));
                }
                Self::Signal(payload[0])
            }
            0x06 => Self::Cancel,
            0x10 => {
                if payload[8] > 1 {
                    return Err(invalid("invalid terminal mode"));
                }
                Self::Ready {
                    session_id: u64::from_be_bytes(payload[..8].try_into().unwrap()),
                    mode: payload[8],
                }
            }
            0x11 => Self::Output(Bytes::copy_from_slice(payload)),
            0x12 => {
                let value = u32::from_be_bytes(payload[1..].try_into().unwrap());
                validate_exit(payload[0], value)?;
                Self::Exit {
                    kind: payload[0],
                    value,
                }
            }
            0x13 => {
                let code = u16::from_be_bytes(payload[..2].try_into().unwrap());
                if !(1..=4).contains(&code) {
                    return Err(invalid("invalid terminal error code"));
                }
                let message = std::str::from_utf8(&payload[2..])
                    .map_err(|_| invalid("invalid terminal error encoding"))?;
                Self::Error {
                    code,
                    message: message.into(),
                }
            }
            _ => unreachable!(),
        };
        input.advance(5 + length);
        Ok(Some(frame))
    }
}

fn validate_size(rows: u16, cols: u16) -> Result<()> {
    if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
        return Err(invalid("terminal dimensions must be in 1..=1000"));
    }
    Ok(())
}

fn validate_open(rows: u16, cols: u16, term: &str, cwd: &str) -> Result<()> {
    validate_size(rows, cols)?;
    if !matches!(term, "xterm-256color" | "xterm" | "vt100" | "dumb") {
        return Err(invalid("unsupported terminal TERM"));
    }
    if cwd.len() > 1024 || cwd.contains(['\0', '\\', '%', ':']) {
        return Err(invalid("invalid terminal cwd"));
    }
    if !cwd.is_empty()
        && cwd
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.len() > 255)
    {
        return Err(invalid("terminal cwd must be a canonical relative path"));
    }
    if matches!(
        cwd.split('/').next().unwrap_or(""),
        ".profile"
            | ".bash_profile"
            | ".bashrc"
            | ".zshenv"
            | ".zprofile"
            | ".zshrc"
            | ".ssh"
            | ".aws"
            | ".gnupg"
            | ".config"
            | "Library"
    ) {
        return Err(invalid("terminal cwd is reserved"));
    }
    Ok(())
}

fn validate_exit(kind: u8, value: u32) -> Result<()> {
    if !match kind {
        0 => value <= 255,
        1 => value > 0,
        2 => (1..=6).contains(&value),
        _ => false,
    } {
        return Err(invalid("invalid terminal exit status"));
    }
    Ok(())
}

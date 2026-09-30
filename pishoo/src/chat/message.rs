use std::fmt;

use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_MESSAGE_LIMIT: u16 = 50;
pub(crate) const MAX_MESSAGE_LIMIT: u16 = 100;
pub(crate) const MAX_MESSAGE_TEXT_CHARS: usize = 4_000;
pub(crate) const MAX_MESSAGE_ID_CHARS: usize = 128;
pub(crate) const MAX_MESSAGE_CURSOR_CHARS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageSubmission {
    pub(crate) client_message_id: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageEnvelope {
    pub(crate) id: String,
    pub(crate) client_message_id: String,
    pub(crate) sender: String,
    pub(crate) recipient: String,
    pub(crate) text: String,
    pub(crate) created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MessageQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageValidationError {
    EmptyClientMessageId,
    InvalidClientMessageId,
    EmptyText,
    TextTooLong,
    TextControlCharacter,
    InvalidCursor,
    CursorTooLong,
    InvalidLimit,
}

impl fmt::Display for MessageValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyClientMessageId => "client_message_id is required",
            Self::InvalidClientMessageId => "client_message_id contains invalid characters",
            Self::EmptyText => "message text is required",
            Self::TextTooLong => "message text is too long",
            Self::TextControlCharacter => "message text contains an invalid control character",
            Self::InvalidCursor => "message cursor contains invalid characters",
            Self::CursorTooLong => "message cursor is too long",
            Self::InvalidLimit => "message limit is out of range",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for MessageValidationError {}

impl MessageSubmission {
    pub(crate) fn validate(&self) -> Result<(), MessageValidationError> {
        validate_client_message_id(&self.client_message_id)?;
        validate_text(&self.text)
    }
}

impl MessageQuery {
    pub(crate) fn new(
        after: Option<String>,
        limit: Option<u16>,
    ) -> Result<Self, MessageValidationError> {
        let after = after.map(validate_cursor).transpose()?;
        let limit = limit.unwrap_or(DEFAULT_MESSAGE_LIMIT);
        if !(1..=MAX_MESSAGE_LIMIT).contains(&limit) {
            return Err(MessageValidationError::InvalidLimit);
        }
        Ok(Self { after, limit })
    }
}

fn validate_client_message_id(value: &str) -> Result<(), MessageValidationError> {
    if value.is_empty() {
        return Err(MessageValidationError::EmptyClientMessageId);
    }
    if value.chars().count() > MAX_MESSAGE_ID_CHARS
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'))
    {
        return Err(MessageValidationError::InvalidClientMessageId);
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), MessageValidationError> {
    if value.trim().is_empty() {
        return Err(MessageValidationError::EmptyText);
    }
    if value.chars().count() > MAX_MESSAGE_TEXT_CHARS {
        return Err(MessageValidationError::TextTooLong);
    }
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(MessageValidationError::TextControlCharacter);
    }
    Ok(())
}

fn validate_cursor(value: String) -> Result<String, MessageValidationError> {
    if value.is_empty() || value.chars().count() > MAX_MESSAGE_CURSOR_CHARS {
        return Err(MessageValidationError::CursorTooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(MessageValidationError::InvalidCursor);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MESSAGE_LIMIT, MAX_MESSAGE_LIMIT, MAX_MESSAGE_TEXT_CHARS, MessageQuery,
        MessageSubmission, MessageValidationError,
    };

    #[test]
    fn submission_accepts_plain_text_and_rejects_unknown_fields() {
        let submission: MessageSubmission =
            serde_json::from_str(r#"{"client_message_id":"01J-test_1","text":"hello\nworld"}"#)
                .expect("valid message submission");
        submission.validate().expect("valid message");
        assert!(
            serde_json::from_str::<MessageSubmission>(
                r#"{"client_message_id":"id","text":"hello","sender":"mallory"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn submission_rejects_empty_oversized_and_control_text() {
        for (text, expected) in [
            ("", MessageValidationError::EmptyText),
            (" \n\t", MessageValidationError::EmptyText),
            ("hello\0world", MessageValidationError::TextControlCharacter),
            (
                &"x".repeat(MAX_MESSAGE_TEXT_CHARS + 1),
                MessageValidationError::TextTooLong,
            ),
        ] {
            let submission = MessageSubmission {
                client_message_id: String::from("id"),
                text: text.to_owned(),
            };
            assert_eq!(submission.validate(), Err(expected));
        }
    }

    #[test]
    fn submission_rejects_invalid_client_message_ids() {
        for id in ["", "has space", "has/slash", "has\nnewline"] {
            let submission = MessageSubmission {
                client_message_id: id.to_owned(),
                text: String::from("hello"),
            };
            assert_eq!(
                submission.validate(),
                Err(if id.is_empty() {
                    MessageValidationError::EmptyClientMessageId
                } else {
                    MessageValidationError::InvalidClientMessageId
                })
            );
        }
    }

    #[test]
    fn query_defaults_and_bounds_limit_and_cursor() {
        let default = MessageQuery::new(None, None).expect("default query");
        assert_eq!(default.limit, DEFAULT_MESSAGE_LIMIT);
        assert_eq!(
            MessageQuery::new(Some(String::from("cursor-1")), Some(MAX_MESSAGE_LIMIT))
                .expect("bounded query")
                .after
                .as_deref(),
            Some("cursor-1")
        );
        assert_eq!(
            MessageQuery::new(None, Some(0)).unwrap_err(),
            MessageValidationError::InvalidLimit
        );
        assert_eq!(
            MessageQuery::new(None, Some(MAX_MESSAGE_LIMIT + 1)).unwrap_err(),
            MessageValidationError::InvalidLimit
        );
        assert_eq!(
            MessageQuery::new(Some(String::from("bad\n")), None).unwrap_err(),
            MessageValidationError::InvalidCursor
        );
    }
}

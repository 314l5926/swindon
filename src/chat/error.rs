use std::io;
use std::str::Utf8Error;

use rustc_serialize::{Encodable, Encoder};
use rustc_serialize::json::{Json, ParserError};
use minihttp::enums::Status;

use super::message::Meta;

quick_error! {
    #[derive(Debug)]
    pub enum MessageError {
        /// Http client request error;
        IoError(err: io::Error) {
            description(err.description())
            display("I/O error: {:?}", err)
            from()
        }
        /// Utf8 decoding error;
        Utf8Error(err: Utf8Error) {
            description(err.description())
            display("Decode error {}", err)
            from()
        }
        /// JSON Parser Error;
        JsonError(err: ParserError) {
            description(err.description())
            display("JSON error: {}", err)
            from()
        }
        /// Protocol Message validation error;
        ValidationError(err: ValidationError) {
            description("Message validation error")
            display("Validation error: {:?}", err)
            from()
        }
        /// Backend application Error;
        HttpError(status: Status, body: Option<Json>) {
            // from()
            description("Http error")
            display("Http error: {}: {:?}", status.code(), body)
        }
    }
}


#[derive(Debug, PartialEq)]
pub enum ValidationError {
    /// Invalid message length;
    InvalidLength,
    /// Invalid method ("tangle." or contains ".");
    InvalidMethod,
    /// request_id is missing or invalid in request_meta object;
    InvalidRequestId,
    /// user_id is missing or invalid in request_meta object;
    InvalidUserId,
    /// Array of args expected;
    ArrayExpected,
    /// Meta/Kwargs object expected;
    ObjectExpected,
}

impl Encodable for MessageError {
    fn encode<S: Encoder>(&self, s: &mut S) -> Result<(), S::Error> {
        use self::MessageError::*;
        match *self {
            HttpError(_, None) => {
                s.emit_nil()
            }
            HttpError(_, Some(ref j)) => {
                j.encode(s)
            }
            Utf8Error(ref err) => {
                s.emit_str(format!("{}", err).as_str())
            }
            JsonError(ref err) => {
                s.emit_str(format!("{}", err).as_str())
            }
            ValidationError(ref err) => {
                s.emit_str(format!("{:?}", err).as_str())
            }
            IoError(ref err) => {
                s.emit_str(format!("{:?}", err).as_str())
            }
        }
    }
}

impl MessageError {
    pub fn update_meta(&self, meta: &mut Meta) {
        use self::MessageError::*;
        let kind = match *self {
            HttpError(s, _) => {
                meta.insert("http_error".to_string(),
                    Json::U64(s.code() as u64));
                "http_error"
            }
            IoError(_) | Utf8Error(_) | JsonError(_) => "invalid_content",
            ValidationError(_) => "protocol_error",
        };
        meta.insert("error_kind".to_string(), Json::String(kind.to_string()));
    }
}
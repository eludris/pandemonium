use deadpool_redis::redis::Msg;
use std::{error::Error, fmt::Display};
use todel::models::Message;

/// An Error that represents a Payload not being found.
#[derive(Debug)]
pub struct PayloadNotFound {}

impl Display for PayloadNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Payload Not Found")
    }
}

impl Error for PayloadNotFound {}

/// A function that simplifies deserializing a message Payload.
pub fn deserialize_message(message: Msg) -> Result<Message, Box<dyn Error + Send + Sync>> {
    Ok(serde_json::from_str::<Message>(
        &message
            .get_payload::<String>()
            .map_err(|_| PayloadNotFound {})?,
    )?)
}

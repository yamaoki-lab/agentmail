pub mod imap;
pub mod runtime;
pub mod upload;

use serde::{Deserialize, Serialize};

pub const DATA_DIRECTORY: &str = ".local/share/agentmail";

pub fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
        })
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Snapshot {
    pub identity: String,
    pub uid_validity: u32,
    pub unread_count: usize,
    pub messages: Vec<Metadata>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Metadata {
    pub uid: u32,
    pub sender: String,
    pub subject: String,
}

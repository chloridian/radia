#![deny(clippy::unwrap_used, clippy::indexing_slicing)]

pub mod inventory;
pub mod lludp;
pub mod login;
pub mod messages;
pub mod types;

pub mod schema {
    include!(concat!(env!("OUT_DIR"), "/schema.rs"));
}

pub use self::lludp::{Audible, BlockInstance, BlockValues, ChatSourceType, ChatType, FieldValue, Frequency, LLUdpError, SchemaMessage};

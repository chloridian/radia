#![deny(clippy::unwrap_used, clippy::indexing_slicing)]

mod decode;
mod encoding;
mod error;
mod format;
mod limits;
mod serialization;
mod value;

pub mod binary;
pub mod notation;
pub mod uuid;
pub mod xml;

pub use self::{
    error::{Error, Result},
    format::from_slice_with_limits,
    limits::Limits,
    value::{Binary, Date, Map, Uri, Value},
};

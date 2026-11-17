mod de;
mod ser;

use std::io::{Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub use self::de::{from_slice_prefix_with_limits, from_slice_with_limits};
use crate::{Limits, Result, encoding::read_limited, serialization};

pub fn from_slice<T: DeserializeOwned>(input: &[u8]) -> Result<T> {
    from_slice_with_limits(input, Limits::default())
}

pub fn from_reader<T: DeserializeOwned, R: Read>(reader: R) -> Result<T> {
    from_reader_with_limits(reader, Limits::default())
}

pub fn from_reader_with_limits<T: DeserializeOwned, R: Read>(mut reader: R, limits: Limits) -> Result<T> {
    let bytes = read_limited(&mut reader, limits.max_bytes)?;
    from_slice_with_limits(&bytes, limits)
}

pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    serialization::to_vec::<ser::Encoder, _>(value, None)
}

pub fn to_vec_headerless<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    serialization::to_vec_headerless::<ser::Encoder, _>(value)
}

pub fn to_writer<T: Serialize + ?Sized, W: Write>(writer: W, value: &T) -> Result<()> {
    serialization::to_writer::<ser::Encoder, _, _>(writer, value)
}

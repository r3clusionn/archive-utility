//! `arx`: an archive format and tool.
//!
//! * [`format`]: the container, its records and their checksums.
//! * [`entry`]: what the records carry and how it is serialized.
//! * [`codec`]: compressing one chunk.
//! * [`pathsafe`]: making stored paths safe to extract.
//! * [`writer`]: creating archives, compressing in parallel.
//! * [`input`]: reading records from a stream and recovering after damage.
//! * [`reader`]: verifying, listing and extracting, from a stream or with random access.

pub mod codec;
pub mod entry;
pub mod format;
pub mod input;
pub mod pathsafe;
pub mod reader;
pub mod writer;

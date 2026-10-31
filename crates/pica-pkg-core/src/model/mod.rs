//! Typed views of the files Pica keeps on a device.
//!
//! Each type names the fields Pica reads and writes, and carries every other
//! field through `extra` unchanged, so a file written by a newer release keeps
//! what this one does not know about. A record's `manifest` stays a JSON value:
//! its keys are whatever the package declared.

pub mod config;
pub mod db;
pub mod local_index;

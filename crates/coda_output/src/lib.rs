//! Bounded output storage shared by foreground, background and programmatic tools.

pub mod archive_dir;
pub mod preview;

pub mod store;
pub use store::Store;
pub mod log;
pub mod render;

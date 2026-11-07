//! Store path names, path-info, and the closure comparison.

pub mod closure;
pub mod log;
pub mod name;
pub mod pathinfo;

pub use closure::{Change, Kind, SIZE_DELTA_THRESHOLD, Summary, diff_paths};
pub use log::Filter as LogFilter;

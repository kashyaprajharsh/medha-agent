//! The terminal's clipboard reader beside the shared image admission.

mod clipboard;

pub(crate) use clipboard::bytes as clipboard_bytes;
pub use runtime::attachments::*;

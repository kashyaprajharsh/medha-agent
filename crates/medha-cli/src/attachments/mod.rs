//! The terminal's clipboard reader beside the shared image admission.

mod clipboard;

pub(crate) use clipboard::{
    Contents as ClipboardContents, read as read_clipboard, text as clipboard_text,
};
pub use runtime::attachments::*;

//! What the desktop transcript shows, shared by the history service and the
//! live bridge so both render a turn the same way: safe Markdown, and each
//! tool step as a verb, a target and readable, bounded text.

mod call;
mod markdown;
mod outcome;
mod steps;
mod stored;
mod tool;

pub use call::{Call, tool_verb};
pub use markdown::to_html;
pub use outcome::Outcome;
pub use steps::Steps;
pub use tool::{clip, failure_detail, plan};

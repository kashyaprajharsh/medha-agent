//! Messaging gateway: platforms such as Telegram reach Medha chats through the same
//! backend the terminal and desktop use. Platforms carry text; this crate owns who may
//! talk, which chat a message belongs to, approvals, questions and files.

mod attachments;
pub mod chats;
mod link;
pub mod one_tap;
pub mod pairing;
pub mod platform;
pub mod platforms;
mod progress;
mod prompts;
mod reply;
mod router;
mod store;
pub mod telegram;
mod telegram_format;

pub use link::Settings;
pub use router::Router;
pub use store::{hold_running, running, running_pid};

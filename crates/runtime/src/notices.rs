//! Start-up remarks for whoever opened the chat: a terminal prints them, a backend routes them.

pub trait Notices: Send + Sync {
    fn say(&self, line: &str);
}

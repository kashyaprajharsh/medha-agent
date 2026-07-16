//! The clarify seam (human-in-the-loop questions). When the agent needs a
//! decision from the user *before* proceeding, the `clarify` tool asks a
//! structured question through this trait — the mirror of [`crate::gate`] for
//! questions rather than yes/no approvals. The kernel knows only the trait; the
//! surface (TUI) renders the form. Headless runs use [`NoAsker`] — no interactive
//! user, so the tool reports "skipped" and the agent proceeds on best judgment.

use async_trait::async_trait;

/// One option the user can pick for a question.
#[derive(Debug, Clone)]
pub struct QOption {
    pub label: String,
    pub description: String,
    /// Marked as the suggested choice (shown with a ★).
    pub recommended: bool,
}

/// A single question put to the user.
#[derive(Debug, Clone)]
pub struct Question {
    /// The full question text.
    pub prompt: String,
    /// A short chip/label (e.g. "Auth method"); may be empty.
    pub header: String,
    pub options: Vec<QOption>,
    /// `true` → checkboxes (any number); `false` → radio (exactly one).
    pub multi_select: bool,
}

/// The user's answer to one question: the chosen option label(s), plus any
/// free-text entered via the "Other" escape.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub selected: Vec<String>,
    pub other: Option<String>,
}

/// Ask the user one or more questions and wait for the answers. `None` means no
/// interactive surface was available (headless) or the user cancelled — callers
/// must treat that as "no answer", never block forever.
#[async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, questions: Vec<Question>) -> Option<Vec<Answer>>;
}

/// No interactive user (headless / non-interactive): every question is skipped.
pub struct NoAsker;

#[async_trait]
impl Asker for NoAsker {
    async fn ask(&self, _questions: Vec<Question>) -> Option<Vec<Answer>> {
        None
    }
}

//! Approvals and questions waiting on this chat, offered as buttons and answered by a
//! button or by a reply with a number. Only choices the backend offered can be sent back.

use crate::platform::Button;
use crate::telegram_format::{code_block, literal};
use protocol::{Answer, AnswerQuestion, ApprovalDecision, ApprovalPrompt, QuestionPrompt};
use std::collections::{BTreeMap, HashMap};

/// Long diffs and commands are cut so the choice stays readable on a phone.
const DETAIL_CHARS: usize = 1500;

pub(crate) struct Ask {
    pub text: String,
    pub buttons: Vec<Button>,
}

/// What a press or a reply does. `said` is the answer just given, in words, for the message
/// that asked.
pub(crate) enum Step {
    Approve {
        gate: u64,
        decision: ApprovalDecision,
    },
    Answer {
        answer: AnswerQuestion,
        said: String,
    },
    Ask {
        question: u64,
        said: String,
        ask: Ask,
    },
}

/// How a waiting approval ended, as this chat knows it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Answered from this chat, with the choice's label.
    Here(String),
    Elsewhere,
}

#[derive(Default)]
pub(crate) struct Prompts {
    approvals: BTreeMap<u64, Vec<(ApprovalDecision, String)>>,
    answered: HashMap<u64, String>,
    question: Option<Asking>,
}

struct Asking {
    id: u64,
    questions: Vec<protocol::Question>,
    answers: Vec<Answer>,
}

impl Prompts {
    pub fn waiting(&self) -> bool {
        !self.approvals.is_empty() || self.question.is_some()
    }

    /// `None` when this approval was offered already: a resumed chat lists it, and it may arrive live too.
    pub fn approval(&mut self, prompt: &ApprovalPrompt) -> Option<Ask> {
        if self.approvals.contains_key(&prompt.gate_id) {
            return None;
        }
        let labels: Vec<String> = prompt.labels().into_iter().map(Into::into).collect();
        let buttons = labels
            .iter()
            .enumerate()
            .map(|(index, label)| Button {
                label: label.clone(),
                choice: format!("a.{}.{index}", prompt.gate_id),
            })
            .collect();
        self.approvals.insert(
            prompt.gate_id,
            prompt.choices.iter().copied().zip(labels).collect(),
        );
        // A command or diff is shown as written: its `*` or `-` lines are not formatting.
        let mut text = match prompt.access {
            Some(_) => literal(&prompt.action),
            None => format!("Medha asks to: {}", literal(&prompt.action)),
        };
        if let Some(detail) = prompt
            .detail
            .as_deref()
            .filter(|detail| !detail.trim().is_empty())
        {
            text.push_str(&format!(
                "\n\n{}",
                code_block(&transcript_view::bounded(detail.trim(), DETAIL_CHARS))
            ));
        }
        Some(Ask { text, buttons })
    }

    /// `None` when the approval was not waiting here.
    pub fn resolved(&mut self, gate: u64) -> Option<Settled> {
        self.approvals.remove(&gate)?;
        Some(match self.answered.remove(&gate) {
            Some(label) => Settled::Here(label),
            None => Settled::Elsewhere,
        })
    }

    pub fn question(&mut self, prompt: QuestionPrompt) -> Option<Ask> {
        if self
            .question
            .as_ref()
            .is_some_and(|asking| asking.id == prompt.question_id)
        {
            return None;
        }
        let asking = Asking {
            id: prompt.question_id,
            questions: prompt.questions,
            answers: Vec::new(),
        };
        let ask = asking.current();
        self.question = ask.is_some().then_some(asking);
        ask
    }

    pub fn question_answered(&mut self, id: u64) -> bool {
        let pending = self.question.as_ref().is_some_and(|asking| asking.id == id);
        if pending {
            self.question = None;
        }
        pending
    }

    pub fn pressed(&mut self, choice: &str) -> Option<Step> {
        let mut parts = choice.split('.');
        match (
            parts.next()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse::<usize>().ok()?,
        ) {
            ("a", gate, index) => self.decide(gate, index),
            ("q", id, option) => {
                let asking = self.question.as_mut().filter(|asking| asking.id == id)?;
                let label = asking
                    .questions
                    .get(asking.answers.len())?
                    .options
                    .get(option)?
                    .label
                    .clone();
                self.answer(Answer {
                    selected: vec![label],
                    other: None,
                })
            }
            _ => None,
        }
    }

    /// A typed reply answers the oldest approval with its number, else the current question.
    pub fn reply(&mut self, text: &str) -> Option<Step> {
        let text = text.trim();
        if let Some((&gate, _)) = self
            .approvals
            .iter()
            .find(|(gate, _)| !self.answered.contains_key(gate))
        {
            let index = text.parse::<usize>().ok()?.checked_sub(1)?;
            return self.decide(gate, index);
        }
        let asking = self.question.as_ref()?;
        let question = asking.questions.get(asking.answers.len())?;
        let numbers: Option<Vec<usize>> = text
            .split([',', ' '])
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.parse::<usize>()
                    .ok()
                    .filter(|n| (1..=question.options.len()).contains(n))
            })
            .collect();
        let answer = match numbers {
            Some(numbers)
                if !numbers.is_empty() && (question.multi_select || numbers.len() == 1) =>
            {
                let mut selected: Vec<String> = Vec::new();
                for number in numbers {
                    let label = &question.options[number - 1].label;
                    if !selected.contains(label) {
                        selected.push(label.clone());
                    }
                }
                Answer {
                    selected,
                    other: None,
                }
            }
            _ => Answer {
                selected: Vec::new(),
                other: Some(text.to_owned()),
            },
        };
        self.answer(answer)
    }

    fn decide(&mut self, gate: u64, index: usize) -> Option<Step> {
        if self.answered.contains_key(&gate) {
            return None;
        }
        let (decision, label) = self.approvals.get(&gate)?.get(index)?.clone();
        self.answered.insert(gate, label);
        Some(Step::Approve { gate, decision })
    }

    fn answer(&mut self, answer: Answer) -> Option<Step> {
        let said = match &answer.other {
            Some(other) => other.clone(),
            None => answer.selected.join(", "),
        };
        let asking = self.question.as_mut()?;
        asking.answers.push(answer);
        if let Some(ask) = asking.current() {
            return Some(Step::Ask {
                question: asking.id,
                said,
                ask,
            });
        }
        let asking = self.question.take()?;
        Some(Step::Answer {
            answer: AnswerQuestion {
                question_id: asking.id,
                dismiss: false,
                answers: asking.answers,
            },
            said,
        })
    }
}

impl Asking {
    fn current(&self) -> Option<Ask> {
        let question = self.questions.get(self.answers.len())?;
        let mut text = if question.header.trim().is_empty() {
            question.prompt.clone()
        } else {
            format!("{}: {}", question.header, question.prompt)
        };
        if question.multi_select {
            for (number, option) in question.options.iter().enumerate() {
                text.push_str(&format!("\n{}. {}", number + 1, described(option)));
            }
            text.push_str(
                "\nReply with the numbers that apply, like 1,3, or type your own answer.",
            );
            return Some(Ask {
                text,
                buttons: Vec::new(),
            });
        }
        for option in question
            .options
            .iter()
            .filter(|option| !option.description.trim().is_empty())
        {
            text.push_str(&format!("\n- {}", described(option)));
        }
        text.push_str("\nOr type your own answer.");
        let buttons = question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| Button {
                label: option.label.clone(),
                choice: format!("q.{}.{index}", self.id),
            })
            .collect();
        Some(Ask { text, buttons })
    }
}

fn described(option: &protocol::QuestionOption) -> String {
    let recommended = if option.recommended {
        " (recommended)"
    } else {
        ""
    };
    match option.description.trim() {
        "" => format!("{}{recommended}", option.label),
        description => format!("{}{recommended}: {description}", option.label),
    }
}

#[cfg(test)]
#[path = "prompts_tests.rs"]
mod tests;

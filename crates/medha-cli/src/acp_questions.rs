//! Desktop question forms retain the waiting tool future until a real answer.
use crate::acp::{Peer, Writer};
use kernel::{Answer, Question};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::oneshot;

type Response = oneshot::Sender<Option<Vec<Answer>>>;
pub(crate) type Questions = Arc<Mutex<HashMap<u64, (Vec<Question>, Response)>>>;

pub(crate) struct AcpAsker {
    pub writer: Arc<Writer>,
    pub pending: Questions,
    pub peer: Peer,
    pub next_id: AtomicU64,
}

struct Guard {
    pending: Questions,
    id: u64,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.id);
    }
}

#[async_trait::async_trait]
impl kernel::Asker for AcpAsker {
    async fn ask(&self, questions: Vec<Question>) -> Option<Vec<Answer>> {
        // Standard ACP has no question extension. The desktop speaks Medha's dialect.
        if self.peer.is_acp() {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let view = questions.iter().map(|question| json!({
            "prompt": question.prompt, "header": question.header, "multi_select": question.multi_select,
            "options": question.options.iter().map(|option| json!({
                "label": option.label, "description": option.description, "recommended": option.recommended,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, (questions, tx));
        let _guard = Guard {
            pending: Arc::clone(&self.pending),
            id,
        };
        if !self
            .writer
            .notify("question", json!({"question_id":id, "questions":view}))
        {
            return None;
        }
        rx.await.ok().flatten()
    }
}

pub(crate) fn clear(pending: &Questions) {
    pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

pub(crate) fn respond(pending: &Questions, params: &Value) -> Result<Value, String> {
    let id = params["question_id"]
        .as_u64()
        .ok_or("question_id required")?;
    let mut pending = pending.lock().map_err(|_| "question state unavailable")?;
    let (questions, _) = pending
        .get(&id)
        .ok_or("This question is no longer waiting.")?;
    let answers = if params["dismiss"] == true {
        None
    } else {
        Some(parse(questions, &params["answers"])?)
    };
    let (_, tx) = pending.remove(&id).ok_or("question unavailable")?;
    tx.send(answers)
        .map_err(|_| "This question is no longer waiting.")?;
    Ok(json!({"accepted":true}))
}

fn parse(questions: &[Question], value: &Value) -> Result<Vec<Answer>, String> {
    let answers = value
        .as_array()
        .filter(|answers| answers.len() == questions.len())
        .ok_or("answer every question")?;
    questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| {
            let selected = answer["selected"]
                .as_array()
                .ok_or("selected options required")?
                .iter()
                .map(|label| {
                    label
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("option label required")
                })
                .collect::<Result<Vec<_>, _>>()?;
            if (!question.multi_select && selected.len() > 1)
                || selected
                    .iter()
                    .any(|label| !question.options.iter().any(|option| &option.label == label))
                || selected
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    != selected.len()
            {
                return Err("invalid option selection".into());
            }
            let other = match answer.get("other") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) if text.len() <= 16_000 => {
                    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
                }
                _ => return Err("custom answer must be text under 16,000 bytes".into()),
            };
            if selected.is_empty() && other.is_none() {
                return Err("choose an option or write an answer".into());
            }
            Ok(Answer { selected, other })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn answers_must_match_the_actual_form() {
        let question = Question {
            prompt: "Pick".into(),
            header: String::new(),
            options: vec![kernel::QOption {
                label: "A".into(),
                description: String::new(),
                recommended: true,
            }],
            multi_select: false,
        };
        assert!(
            parse(
                std::slice::from_ref(&question),
                &json!([{"selected":["B"]}])
            )
            .is_err()
        );
        assert!(
            parse(
                std::slice::from_ref(&question),
                &json!([{"selected":["A","A"]}])
            )
            .is_err()
        );
        assert!(parse(std::slice::from_ref(&question), &json!([{"selected":[]}])).is_err());
        let answer = parse(&[question], &json!([{"selected":[], "other":"Custom"}])).unwrap();
        assert_eq!(answer[0].other.as_deref(), Some("Custom"));
    }
    #[test]
    fn dismissal_releases_a_waiting_question() {
        let pending = Questions::default();
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(1, (vec![], tx));
        respond(&pending, &json!({"question_id":1,"dismiss":true})).unwrap();
        assert!(rx.try_recv().unwrap().is_none());
        assert!(pending.lock().unwrap().is_empty());
    }
}

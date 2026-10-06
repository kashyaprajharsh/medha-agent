//! Bounded, recoverable presentation owned by a chat, never by its viewers.
//! Mutations and their output frames use Writer's common ordering lock.

use protocol::{AgentStep, PresentationItem as Item, TurnEvent};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

const ROW_BYTES: usize = 256 * 1024;
const HISTORY_BYTES: usize = 4 * 1024 * 1024;
const AGENT_BYTES: usize = 4 * 1024 * 1024;
const FORM_BYTES: usize = 2 * 1024 * 1024;
const MAX_AGENTS: usize = 128;

struct Count(usize);
impl std::io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn size(value: &impl Serialize) -> usize {
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("presentation serializes");
    count.0
}

pub(crate) fn history(messages: &[kernel::Message]) -> (Vec<Item>, u64) {
    let mut presentation = Presentation::default();
    presentation.seed_history(messages);
    (presentation.rows.snapshot(), presentation.rows.omitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings() -> protocol::Settings {
        serde_json::from_value(json!({"profiles": [], "profile": "test", "model": "test",
            "mode": "careful", "reasoning": "auto", "effort": "auto", "efforts": [],
            "reasoning_support": "unverified", "streaming": true, "context_limit": null}))
        .unwrap()
    }

    fn observe(state: &mut Presentation, event: TurnEvent) {
        state
            .observe("event", serde_json::to_value(event).unwrap())
            .unwrap();
    }

    #[test]
    fn retries_preserve_durable_rows_and_do_not_join_an_older_answer() {
        let mut state = Presentation::default();
        state.seed(
            "one".into(),
            &[
                kernel::Message::user("before"),
                kernel::Message::new(kernel::Role::Assistant, "old answer"),
            ],
            settings(),
        );
        observe(&mut state, TurnEvent::Started { turn: 1 });
        observe(
            &mut state,
            TurnEvent::Text {
                delta: "failed attempt".into(),
            },
        );
        observe(
            &mut state,
            TurnEvent::ToolCall {
                id: Some("a".into()),
                tool: "read".into(),
                args: json!({"path": "a"}),
            },
        );
        observe(
            &mut state,
            TurnEvent::ToolResult {
                id: Some("a".into()),
                tool: "read".into(),
                ok: true,
                payload: json!("kept"),
            },
        );
        observe(
            &mut state,
            TurnEvent::Reasoning {
                delta: "discarded reasoning".into(),
            },
        );
        observe(&mut state, TurnEvent::Restarted);
        observe(
            &mut state,
            TurnEvent::Text {
                delta: "successful attempt".into(),
            },
        );
        let rows = state.snapshot().items;
        assert!(rows.contains(&Item::Assistant {
            text: "old answer".into()
        }));
        assert!(rows.contains(&Item::Assistant {
            text: "successful attempt".into()
        }));
        assert!(rows.iter().any(
            |row| matches!(row, Item::ToolResult { payload, .. } if *payload == json!("kept"))
        ));
        assert!(!rows.iter().any(|row| matches!(row, Item::Assistant { text } | Item::Reasoning { text } if text.contains("discarded") || text.contains("failed"))));
    }

    #[test]
    fn eviction_keeps_the_retry_boundary_at_the_correct_row() {
        let mut rows = Rows::default();
        rows.push(Item::Assistant { text: "old".into() });
        rows.push(Item::ToolCall {
            id: None,
            tool: "before".into(),
            args: json!({}),
        });
        rows.attempt = rows.items.len();
        rows.push(Item::Assistant {
            text: "retry".into(),
        });
        rows.evict();
        rows.restart();
        assert!(matches!(&rows.items[0].0, Item::ToolCall { tool, .. } if tool == "before"));
        assert!(
            !rows
                .items
                .iter()
                .any(|(row, _)| matches!(row, Item::Assistant { .. }))
        );
    }

    #[test]
    fn usage_never_counts_unknown_cache_as_a_cache_miss() {
        let mut state = Presentation::default();
        for cached in [None, Some(80), Some(150)] {
            observe(
                &mut state,
                TurnEvent::Usage {
                    prompt_tokens: 100,
                    total_tokens: 120,
                    completion_tokens: Some(20),
                    cached_prompt_tokens: cached,
                },
            );
        }
        let metrics = state.snapshot().metrics;
        assert_eq!(metrics.prompt_tokens, 300);
        assert_eq!(metrics.cached_prompt_tokens, Some(180));
        assert_eq!(metrics.cache_prompt_tokens, Some(200));
        assert_eq!(metrics.cache_unreported_attempts, 1);
    }

    #[test]
    fn duplicate_steers_settle_one_copy_at_a_time_and_returned_text_stays_visible() {
        let mut state = Presentation::default();
        for _ in 0..2 {
            observe(
                &mut state,
                TurnEvent::Queued {
                    content: Some("same".into()),
                },
            );
        }
        observe(
            &mut state,
            TurnEvent::Steered {
                content: "same".into(),
            },
        );
        assert_eq!(state.snapshot().pending_steers, vec!["same"]);
        observe(
            &mut state,
            TurnEvent::Returned {
                contents: vec!["same".into()],
            },
        );
        assert!(state.snapshot().pending_steers.is_empty());
        assert_eq!(
            state.snapshot().items,
            vec![Item::User {
                text: "same".into()
            }]
        );
    }

    #[test]
    fn safety_forms_are_recoverable_and_settle_across_viewers() {
        let mut state = Presentation::default();
        let approval = json!({"gate_id": 1, "kind": "action", "action": "shell.exec", "detail": "echo harmless", "escalated": false});
        let question = json!({"question_id": 2, "questions": [{"header": "Choice", "prompt": "Which?", "multi_select": false, "options": [{"label": "A", "description": "First", "recommended": false}]}]});
        // Use real protocol shapes; invalid safety forms fail rather than vanish.
        let approval: protocol::ApprovalPrompt = serde_json::from_value(approval).unwrap();
        state
            .observe("approval", serde_json::to_value(&approval).unwrap())
            .unwrap();
        let question: protocol::QuestionPrompt = serde_json::from_value(question).unwrap();
        state
            .observe("question", serde_json::to_value(&question).unwrap())
            .unwrap();
        assert_eq!(state.snapshot().approvals.len(), 1);
        assert_eq!(state.snapshot().questions.len(), 1);
        state
            .observe(
                "approval.resolved",
                json!({"gate_id": 1, "approved": false}),
            )
            .unwrap();
        state
            .observe(
                "question.answered",
                json!({"question_id": 2, "answered": true}),
            )
            .unwrap();
        assert!(state.snapshot().approvals.is_empty());
        assert!(state.snapshot().questions.is_empty());
        assert_eq!(state.form_bytes, 0);
    }

    #[test]
    fn bounded_history_and_child_views_disclose_omission_without_allocating_forever() {
        let mut state = Presentation::default();
        state.seed("parent".into(), &[], settings());
        for index in 0..150 {
            state
                .observe(
                    "agent.step",
                    serde_json::to_value(protocol::AgentEvent {
                        surface_session: Some("parent".into()),
                        path: format!("root/child-{index}"),
                        step: AgentStep::Text("x".repeat(100_000)),
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        assert_eq!(state.agents.len(), MAX_AGENTS);
        assert!(state.snapshot().omitted_agent_views > 0);
        assert!(
            state
                .agents
                .values()
                .map(|pane| pane.rows.bytes)
                .sum::<usize>()
                <= AGENT_BYTES
        );
        observe(
            &mut state,
            TurnEvent::ToolResult {
                id: Some("large".into()),
                tool: "read".into(),
                ok: true,
                payload: json!("x".repeat(ROW_BYTES)),
            },
        );
        assert!(state.snapshot().items.iter().any(
            |item| matches!(item, Item::PreviewOmitted { id: Some(id), .. } if id == "large")
        ));
        assert!(size(&state.snapshot()) < 12 * 1024 * 1024);
        let before = state.agents.len();
        state
            .observe(
                "agent.step",
                serde_json::to_value(protocol::AgentEvent {
                    surface_session: Some("old parent".into()),
                    path: "stale".into(),
                    step: AgentStep::Text("stale".into()),
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(state.agents.len(), before);
        assert!(!state.agents.contains_key("stale"));
    }
}

#[derive(Default)]
struct Rows {
    items: VecDeque<(Item, usize)>,
    bytes: usize,
    omitted: u64,
    attempt: usize,
}

impl Rows {
    fn evict(&mut self) -> bool {
        let Some((_, bytes)) = self.items.pop_front() else {
            return false;
        };
        self.bytes -= bytes;
        self.omitted += 1;
        self.attempt = self.attempt.saturating_sub(1);
        true
    }

    fn bound(&mut self, count: usize, bytes: usize) {
        while self.items.len() > count || self.bytes > bytes {
            if !self.evict() {
                break;
            }
        }
    }

    fn push(&mut self, item: Item) {
        let bytes = size(&item);
        let item = if bytes > ROW_BYTES {
            let (subject, id) = match &item {
                Item::ToolCall { id, tool, .. } | Item::ToolResult { id, tool, .. } => {
                    (tool.clone(), id.clone())
                }
                Item::User { .. } => ("message".into(), None),
                Item::Assistant { .. } => ("assistant reply".into(), None),
                Item::Reasoning { .. } => ("reasoning".into(), None),
                _ => ("output".into(), None),
            };
            Item::PreviewOmitted { subject, id, bytes }
        } else {
            item
        };
        let bytes = if bytes > ROW_BYTES {
            size(&item)
        } else {
            bytes
        };
        self.bytes += bytes;
        self.items.push_back((item, bytes));
        self.bound(5000, HISTORY_BYTES);
    }

    fn text(&mut self, delta: &str, thinking: bool) {
        // Count only the delta's JSON escaping, rather than reserializing an
        // ever-growing answer on every token (quadratic work).
        let added = size(&delta).saturating_sub(2);
        if self.items.len() > self.attempt
            && let Some((last, bytes)) = self.items.back_mut()
        {
            let text = match (last, thinking) {
                (Item::Assistant { text }, false) | (Item::Reasoning { text }, true) => Some(text),
                _ => None,
            };
            if let Some(text) = text
                && *bytes + added <= ROW_BYTES
            {
                text.push_str(delta);
                *bytes += added;
                self.bytes += added;
                self.bound(5000, HISTORY_BYTES);
                return;
            }
        }
        self.push(if thinking {
            Item::Reasoning { text: delta.into() }
        } else {
            Item::Assistant { text: delta.into() }
        });
    }

    fn restart(&mut self) {
        let mut index = 0;
        let mut removed = 0;
        self.items.retain(|(item, bytes)| {
            let streamed = matches!(item, Item::Assistant { .. } | Item::Reasoning { .. })
                || matches!(item, Item::PreviewOmitted { subject, .. } if subject == "assistant reply" || subject == "reasoning");
            let keep = index < self.attempt || !streamed;
            index += 1;
            if !keep { removed += bytes; }
            keep
        });
        self.bytes -= removed;
        self.push(Item::Notice {
            text: "the model's connection dropped — retrying".into(),
        });
    }

    fn snapshot(&self) -> Vec<Item> {
        self.items.iter().map(|(item, _)| item.clone()).collect()
    }
}

struct Pane {
    surface: Option<String>,
    rows: Rows,
    seen: u64,
    pending: usize,
}

#[derive(Default)]
pub(crate) struct Presentation {
    revision: u64,
    conversation: String,
    running: bool,
    turn: u64,
    pending_steers: Vec<String>,
    force_aborting: bool,
    settings: Option<protocol::Settings>,
    rows: Rows,
    metrics: protocol::PresentationMetrics,
    approvals: BTreeMap<u64, protocol::ApprovalPrompt>,
    questions: BTreeMap<u64, protocol::QuestionPrompt>,
    agents: BTreeMap<String, Pane>,
    omitted_agents: u64,
    form_bytes: usize,
}

impl Presentation {
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn seed(
        &mut self,
        conversation: String,
        messages: &[kernel::Message],
        settings: protocol::Settings,
    ) {
        if self.conversation != conversation {
            self.agents.clear();
            self.omitted_agents = 0;
            self.metrics = protocol::PresentationMetrics::default();
            self.pending_steers.clear();
        }
        self.conversation = conversation;
        self.settings = Some(settings);
        self.seed_history(messages);
        self.revision += 1;
    }

    fn seed_history(&mut self, messages: &[kernel::Message]) {
        self.rows = Rows::default();
        self.metrics.context_pressure = None;
        let mut tools = BTreeMap::new();
        let mut tool_ids = VecDeque::new();
        for message in messages {
            match message.role {
                kernel::Role::User
                    if message
                        .trust
                        .is_none_or(|trust| trust == kernel::TrustLabel::User) =>
                {
                    self.rows.push(Item::User {
                        text: message.content.clone(),
                    })
                }
                kernel::Role::Assistant => {
                    if !message.content.trim().is_empty() {
                        self.rows.push(Item::Assistant {
                            text: message.content.clone(),
                        });
                    }
                    for call in &message.tool_calls {
                        tools.insert(call.id.clone(), call.tool.clone());
                        tool_ids.push_back(call.id.clone());
                        if tool_ids.len() > 5000
                            && let Some(old) = tool_ids.pop_front()
                        {
                            tools.remove(&old);
                        }
                        self.rows.push(Item::ToolCall {
                            id: Some(call.id.clone()),
                            tool: call.tool.clone(),
                            args: call.args.clone(),
                        });
                    }
                }
                kernel::Role::Tool => {
                    let tool = message
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| tools.get(id))
                        .cloned()
                        .unwrap_or_else(|| "tool".into());
                    let payload = serde_json::from_str(&message.content)
                        .unwrap_or_else(|_| Value::String(message.content.clone()));
                    let ok = crate::result_failure(&tool, &payload).is_none();
                    self.rows.push(Item::ToolResult {
                        id: message.tool_call_id.clone(),
                        tool,
                        ok,
                        payload,
                    });
                }
                _ => {}
            }
        }
    }

    fn event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Started { turn } => {
                self.turn = turn;
                self.running = true;
                self.force_aborting = false;
                self.metrics.reasoning_received_this_turn = false;
                self.rows.attempt = self.rows.items.len();
            }
            TurnEvent::Steered { content } => {
                if let Some(index) = self
                    .pending_steers
                    .iter()
                    .position(|queued| *queued == content)
                {
                    self.pending_steers.remove(index);
                }
                self.rows.push(Item::User { text: content });
            }
            TurnEvent::User { content } => self.rows.push(Item::User { text: content }),
            TurnEvent::Queued {
                content: Some(content),
            } => self.pending_steers.push(content),
            TurnEvent::Returned { contents } => {
                for text in &contents {
                    if let Some(index) =
                        self.pending_steers.iter().position(|queued| queued == text)
                    {
                        self.pending_steers.remove(index);
                    }
                }
            }
            TurnEvent::Text { delta } => self.rows.text(&delta, false),
            TurnEvent::Reasoning { delta } => {
                self.metrics.reasoning_received_this_turn |= !delta.is_empty();
                self.rows.text(&delta, true);
            }
            TurnEvent::ToolStarted { tool, target } => {
                self.metrics.current_tool = Some((tool, target))
            }
            TurnEvent::ToolCall { id, tool, args } => {
                self.rows.push(Item::ToolCall { id, tool, args })
            }
            TurnEvent::ToolResult {
                id,
                tool,
                ok,
                payload,
            } => {
                self.metrics.current_tool = None;
                self.rows.push(Item::ToolResult {
                    id,
                    tool,
                    ok,
                    payload,
                });
            }
            TurnEvent::Notice { text } => self.rows.push(Item::Notice { text }),
            TurnEvent::Compaction {
                before,
                after,
                summarized,
                summary,
            } => self.rows.push(Item::Compaction {
                before,
                after,
                summarized,
                summary,
            }),
            TurnEvent::Verify { ok, summary } => self.rows.push(Item::Verify { ok, summary }),
            TurnEvent::Compacting { active } => self.metrics.compacting = active,
            TurnEvent::ContextPressure {
                input_tokens,
                input_limit,
                usable_input_tokens,
                quality,
                ..
            } => {
                self.metrics.context_pressure = Some(protocol::ContextSnapshot {
                    input_tokens,
                    input_limit,
                    usable_input_tokens,
                    quality,
                })
            }
            TurnEvent::Usage {
                prompt_tokens,
                total_tokens,
                completion_tokens,
                cached_prompt_tokens,
            } => {
                self.metrics.prompt_tokens = self
                    .metrics
                    .prompt_tokens
                    .saturating_add(u64::from(prompt_tokens));
                if let Some(cached) = cached_prompt_tokens {
                    let hits = self.metrics.cached_prompt_tokens.get_or_insert(0);
                    *hits = hits.saturating_add(u64::from(cached.min(prompt_tokens)));
                    let prompt = self.metrics.cache_prompt_tokens.get_or_insert(0);
                    *prompt = prompt.saturating_add(u64::from(prompt_tokens));
                } else {
                    self.metrics.cache_unreported_attempts =
                        self.metrics.cache_unreported_attempts.saturating_add(1);
                }
                self.metrics.last_usage = Some(protocol::UsageSnapshot {
                    prompt_tokens,
                    total_tokens,
                    completion_tokens,
                    cached_prompt_tokens,
                });
            }
            TurnEvent::Cost {
                total_usd,
                indicative,
            } => {
                self.metrics.cost_usd = Some(total_usd);
                self.metrics.cost_indicative = indicative;
            }
            TurnEvent::Restarted => {
                self.rows.restart();
                self.metrics.compacting = false;
                self.metrics.current_tool = None;
                self.metrics.reasoning_received_this_turn = false;
            }
            TurnEvent::AbortSlow => self.force_aborting = true,
            TurnEvent::AbortSettled => self.force_aborting = false,
            TurnEvent::Done { .. } | TurnEvent::Cancelled | TurnEvent::Error { .. } => {
                self.running = false;
                self.metrics.last_turn_reasoning_received =
                    Some(self.metrics.reasoning_received_this_turn);
                self.metrics.compacting = false;
                self.metrics.current_tool = None;
            }
            _ => {}
        }
    }

    pub(crate) fn observe(&mut self, method: &str, params: Value) -> Result<(), &'static str> {
        match method {
            "event" => {
                let event =
                    serde_json::from_value(params).map_err(|_| "Invalid turn presentation.")?;
                self.event(event);
            }
            "settings" => {
                self.settings = Some(
                    serde_json::from_value(params).map_err(|_| "Invalid settings presentation.")?,
                )
            }
            "approval" => {
                let prompt: protocol::ApprovalPrompt =
                    serde_json::from_value(params).map_err(|_| "Invalid approval presentation.")?;
                self.form_bytes += size(&prompt);
                if let Some(previous) = self.approvals.insert(prompt.gate_id, prompt) {
                    self.form_bytes -= size(&previous);
                }
            }
            "approval.resolved" => {
                if let Some(id) = params["gate_id"].as_u64()
                    && let Some(prompt) = self.approvals.remove(&id)
                {
                    self.form_bytes -= size(&prompt);
                }
            }
            "question" => {
                let prompt: protocol::QuestionPrompt =
                    serde_json::from_value(params).map_err(|_| "Invalid question presentation.")?;
                self.form_bytes += size(&prompt);
                if let Some(previous) = self.questions.insert(prompt.question_id, prompt) {
                    self.form_bytes -= size(&previous);
                }
            }
            "question.answered" => {
                if let Some(id) = params["question_id"].as_u64()
                    && let Some(prompt) = self.questions.remove(&id)
                {
                    self.form_bytes -= size(&prompt);
                }
            }
            "agent.step" => {
                {
                    let event = serde_json::from_value::<protocol::AgentEvent>(params)
                        .map_err(|_| "Invalid child presentation.")?;
                    if event.surface_session.as_ref().is_some_and(|owner| {
                        !self.conversation.is_empty() && *owner != self.conversation
                    }) {
                        return Ok(());
                    }
                    if self.agents.len() >= MAX_AGENTS && !self.agents.contains_key(&event.path) {
                        let old = self
                            .agents
                            .iter()
                            .filter(|(_, pane)| pane.pending == 0)
                            .min_by_key(|(_, pane)| pane.seen)
                            .map(|(path, _)| path.clone())
                            .ok_or("Too many active child views to recover safely.")?;
                        self.agents.remove(&old);
                        self.omitted_agents += 1;
                    }
                    let pane = self.agents.entry(event.path).or_insert_with(|| Pane {
                        surface: event.surface_session,
                        rows: Rows::default(),
                        seen: self.revision,
                        pending: 0,
                    });
                    pane.seen = self.revision;
                    match event.step {
                        AgentStep::Task { objective, .. } => {
                            pane.rows.push(Item::User { text: objective });
                            pane.rows.attempt = pane.rows.items.len();
                        }
                        AgentStep::Text(delta) => pane.rows.text(&delta, false),
                        AgentStep::Reasoning(delta) => pane.rows.text(&delta, true),
                        AgentStep::ToolCall { id, tool, args } => {
                            pane.rows.push(Item::ToolCall { id, tool, args })
                        }
                        AgentStep::ToolResult {
                            id,
                            tool,
                            ok,
                            payload,
                        } => pane.rows.push(Item::ToolResult {
                            id,
                            tool,
                            ok,
                            payload,
                        }),
                        AgentStep::Restarted => {
                            pane.rows.attempt = pane
                                .rows
                                .items
                                .iter()
                                .rposition(|(item, _)| {
                                    matches!(
                                        item,
                                        Item::User { .. }
                                            | Item::ToolCall { .. }
                                            | Item::ToolResult { .. }
                                            | Item::Compaction { .. }
                                            | Item::Verify { .. }
                                    )
                                })
                                .map_or(0, |index| index + 1);
                            pane.rows.restart();
                        }
                        AgentStep::SteerQueued(_) => pane.pending = pane.pending.saturating_add(1),
                        AgentStep::Steered(text) => {
                            pane.pending = pane.pending.saturating_sub(1);
                            pane.rows.push(Item::User { text });
                        }
                        AgentStep::SteersReturned(texts) => {
                            pane.pending = pane.pending.saturating_sub(texts.len());
                        }
                    }
                    pane.rows.bound(200, HISTORY_BYTES);
                    while self
                        .agents
                        .values()
                        .map(|pane| pane.rows.bytes)
                        .sum::<usize>()
                        > AGENT_BYTES
                    {
                        // Reclaim from the largest pane without making one busy
                        // child evict every other child's latest work.
                        let key = self
                            .agents
                            .iter()
                            .max_by_key(|(_, pane)| pane.rows.bytes)
                            .map(|(key, _)| key.clone())
                            .expect("nonempty panes");
                        self.agents
                            .get_mut(&key)
                            .expect("selected pane")
                            .rows
                            .evict();
                    }
                }
            }
            _ => return Ok(()),
        }
        if self.approvals.len() + self.questions.len() > 128 || self.form_bytes > FORM_BYTES {
            // Never omit a safety prompt and pretend it can still be answered.
            return Err("The chat has too many pending questions to recover safely.");
        }
        if self.pending_steers.len() > 128
            || size(&self.pending_steers) > FORM_BYTES
            || self.agents.values().any(|pane| pane.pending > 128)
        {
            return Err("The chat has too much unapplied text to recover safely.");
        }
        self.revision += 1;
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> protocol::PresentationSnapshot {
        protocol::PresentationSnapshot {
            revision: self.revision,
            conversation: self.conversation.clone(),
            running: self.running,
            turn: self.turn,
            pending_steers: self.pending_steers.clone(),
            force_aborting: self.force_aborting,
            settings: self.settings.clone(),
            items: self.rows.snapshot(),
            omitted_items: self.rows.omitted,
            approvals: self.approvals.values().cloned().collect(),
            questions: self.questions.values().cloned().collect(),
            metrics: self.metrics.clone(),
            agents: self
                .agents
                .iter()
                .map(|(path, pane)| protocol::AgentPresentation {
                    surface_session: pane.surface.clone(),
                    path: path.clone(),
                    items: pane.rows.snapshot(),
                    omitted_items: pane.rows.omitted,
                    pending_steers: pane.pending,
                })
                .collect(),
            omitted_agent_views: self.omitted_agents,
            cursor: None,
        }
    }
}

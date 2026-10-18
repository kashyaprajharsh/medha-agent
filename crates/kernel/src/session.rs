use super::*;
use std::ops::ControlFlow;

mod boundary;
mod checkpoint;
mod completion;
mod dispatch;
mod history;
mod images;
mod prepare;
mod recovery;
mod response;
mod start;

type Stopped = (Vec<Message>, StopReason);
type Flow<T> = Result<ControlFlow<Stopped, T>, KernelError>;
type Turn = (
    Message,
    ModelMessage,
    Vec<ToolIntent>,
    Option<crate::types::Usage>,
    bool,
);
type Prepared = (crate::PreparedModelRequest, Option<u64>, Option<u64>);

struct History {
    messages: Vec<Message>,
    ordered_messages: Vec<ModelMessage>,
    logged_events: Vec<Event>,
    window_events: Vec<ulid::Ulid>,
    window_taint: TrustLabel,
    tool_snapshot: crate::tool_snapshot::ToolSnapshot,
    specs: Vec<crate::ToolSpec>,
    checkpoint_policy: Option<u64>,
    check_checkpoint_policy: bool,
    history_scope: Option<String>,
    restored_usage: bool,
    usage_anchor: Option<crate::usage_reporting::UsageAnchor>,
    retained_summary: Option<String>,
    web_tainted: bool,
    has_checkpoint: bool,
}

struct RunState {
    gov: crate::budgets::Governor,
    images_left_out_said: bool,
    retired_said: usize,
    unreadable_said: bool,
    cut_offs: u32,
    join: crate::seam::Join,
    waits: u32,
    empty_nudged: bool,
    empty_tries: u32,
    empty_was_silent: bool,
    tidied_after_text: bool,
    stood_in: Option<(String, String)>,
    image_floor: Sending,
    image_route_reported: bool,
    new_image_submission: bool,
    hook_continuations: u8,
}

struct RequestState {
    overflow_retried: bool,
    go_on: Option<GoOn>,
    sending: Sending,
    sent: Sent,
    image_refusal: Option<String>,
    small_refused: bool,
    unfit_named: bool,
    tool_call_corrections: u32,
    compaction_passes: u32,
}

impl RequestState {
    fn new(sending: Sending) -> Self {
        Self {
            overflow_retried: false,
            go_on: None,
            sending,
            sent: Sent::default(),
            image_refusal: None,
            small_refused: false,
            unfit_named: false,
            tool_call_corrections: 0,
            compaction_passes: 0,
        }
    }
}

struct Ready {
    history: History,
    state: RunState,
    cancel: tokio_util::sync::CancellationToken,
    request_owner: crate::RequestScope,
    started: f64,
    planning: bool,
}

struct RunContext<'a> {
    session: &'a Session,
    sink: &'a dyn StreamSink,
    cancel: &'a tokio_util::sync::CancellationToken,
    request_owner: &'a crate::RequestScope,
    started: f64,
    planning: bool,
}

#[allow(
    clippy::large_enum_variant,
    reason = "Keep recovered replies owned without adding an allocation."
)]
enum Recovery {
    Retry,
    NextTurn,
    Keep(Turn, Vec<String>),
}

struct Accepted {
    intents: Vec<ToolIntent>,
    usage: Option<crate::types::Usage>,
    empty: bool,
    thought: bool,
    said_again: bool,
}

struct FailedRequest<'a> {
    prepared: &'a crate::PreparedModelRequest,
    prepared_input_tokens: Option<u64>,
    error: KernelError,
}

pub(super) async fn run<P: Provider, L: EventLog>(
    kernel: &Kernel<P, L>,
    session: &Session,
    messages: Vec<Message>,
    budget: crate::budgets::Budget,
    sink: &dyn StreamSink,
    interrupts: &mut Option<crate::interrupts::InterruptQueue>,
) -> Result<Stopped, KernelError> {
    let ready = match kernel
        .begin_run(session, messages, budget, sink, interrupts)
        .await?
    {
        ControlFlow::Break(stopped) => return Ok(stopped),
        ControlFlow::Continue(ready) => ready,
    };
    let Ready {
        mut history,
        mut state,
        cancel,
        request_owner,
        started,
        planning,
    } = ready;
    let cx = RunContext {
        session,
        sink,
        cancel: &cancel,
        request_owner: &request_owner,
        started,
        planning,
    };
    'turns: loop {
        if let ControlFlow::Break(stopped) = kernel
            .turn_boundary(&cx, &mut history, &mut state, interrupts)
            .await?
        {
            return Ok(stopped);
        }
        let compile_control = crate::CompileControl::new(cancel.clone(), state.gov.deadline());
        let mut request = RequestState::new(state.image_floor);
        let (turn, request_tools) = loop {
            let (prepared, input, output) = match kernel
                .prepare_turn(
                    &cx,
                    &mut history,
                    &mut state,
                    &mut request,
                    interrupts,
                    &compile_control,
                )
                .await?
            {
                ControlFlow::Break(stopped) => return Ok(stopped),
                ControlFlow::Continue(prepared) => prepared,
            };
            let wall_deadline = state.gov.deadline();
            match kernel
                .run_turn(
                    session,
                    &prepared,
                    input,
                    output,
                    &mut state.gov,
                    sink,
                    &cancel,
                    wall_deadline,
                    &mut state.join,
                )
                .await
            {
                Ok(turn) => {
                    break kernel.accept_request(&cx, &mut state, &mut request, &prepared, turn);
                }
                Err(error) => match kernel
                    .recover_turn(
                        &cx,
                        &mut history,
                        &mut state,
                        &mut request,
                        interrupts,
                        FailedRequest {
                            prepared: &prepared,
                            prepared_input_tokens: input,
                            error,
                        },
                    )
                    .await?
                {
                    ControlFlow::Break(stopped) => return Ok(stopped),
                    ControlFlow::Continue(Recovery::Retry) => continue,
                    ControlFlow::Continue(Recovery::NextTurn) => continue 'turns,
                    ControlFlow::Continue(Recovery::Keep(turn, tools)) => break (turn, tools),
                },
            }
        };
        let accepted = match kernel
            .save_response(
                &cx,
                &mut history,
                &mut state,
                &mut request,
                interrupts,
                turn,
            )
            .await?
        {
            ControlFlow::Break(stopped) => return Ok(stopped),
            ControlFlow::Continue(accepted) => accepted,
        };
        match kernel
            .continue_response(&cx, &mut history, &mut state, &mut request)
            .await?
        {
            ControlFlow::Break(stopped) => return Ok(stopped),
            ControlFlow::Continue(true) => continue,
            ControlFlow::Continue(false) => {}
        }
        match kernel
            .retry_empty_response(&cx, &mut history, &mut state, interrupts, &accepted)
            .await?
        {
            ControlFlow::Break(stopped) => return Ok(stopped),
            ControlFlow::Continue(true) => continue,
            ControlFlow::Continue(false) => {}
        }
        let finishing = accepted.intents.is_empty();
        let modified_files = kernel
            .dispatch_turn(
                &cx,
                &mut history,
                &mut state,
                accepted.intents,
                &request_tools,
            )
            .await?;
        if let ControlFlow::Break(stopped) = kernel
            .complete_turn(
                &cx,
                &mut history,
                &mut state,
                interrupts,
                finishing,
                modified_files,
            )
            .await?
        {
            return Ok(stopped);
        }
    }
}

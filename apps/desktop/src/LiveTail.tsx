import { Fragment, memo } from "react";
import { Markdown } from "./Markdown";
import { Icon } from "./Icon";
import {
  explainError,
  queuedMessages,
  type LiveItem,
  type Approval,
  type LiveAgent,
  type LiveState,
  type QuestionForm,
} from "./live";
import { LiveAgents } from "./LiveAgents";
import { Questions } from "./Questions";
import { Spinner } from "./Spinner";
import { PlanCard, Reasoning, StepGroup } from "./Steps";
import { clock } from "./timeline";
import { TurnHead } from "./Transcript";

type Props = {
  state: LiveState;
  showReasoning?: boolean;
  onAnswer: (
    form: QuestionForm,
    answers?: { selected: string[]; other?: string }[],
  ) => void;
  onApprove: (approval: Approval, allow: boolean) => void;
  openSession?: string | null;
  onOpenAgent: (agent: LiveAgent) => void;
};

export const LiveTail = memo(function LiveTail({
  state,
  onApprove,
  onAnswer,
  showReasoning,
  openSession,
  onOpenAgent,
}: Props) {
  const working =
    state.status === "running" ||
    (state.status === "starting" && state.sent.length > 0);
  const last = state.items.at(-1);
  const queued = queuedMessages(state);
  let replying = false;
  const gap =
    working &&
    state.approvals.length === 0 &&
    state.questions.length === 0 &&
    (!last ||
      last.kind === "user" ||
      queued.length > 0 ||
      (last.kind === "tools" &&
        !last.steps.some((step) => step.status === "running")) ||
      (last.kind === "reasoning" &&
        (last.ended !== undefined || !showReasoning)));
  const gapLabel =
    state.status === "starting"
      ? "Starting Medha…"
      : last && last.kind !== "reasoning"
        ? "Working"
        : "Thinking";
  return (
    <div className="live-tail" aria-live="polite">
      {state.items.map((item, index) => {
        const visible = item.kind !== "reasoning" || showReasoning;
        const first = item.kind !== "user" && !replying && visible;
        if (item.kind === "user") replying = false;
        else if (first) replying = true;
        return (
          <Fragment key={item.id}>
            {first && <TurnHead />}
            {renderItem(item, index)}
          </Fragment>
        );
      })}
      {queued.map((message) => (
        <UserMessage key={message.id} message={message} pending />
      ))}
      {(queued.length > 0 || !replying) &&
        (working || state.approvals.length > 0) && <TurnHead />}
      {state.approvals.map((approval) => (
        <div
          className="ask"
          key={approval.gateId}
          role="group"
          aria-label="Approval needed"
        >
          <div className="ask-top">
            <Icon name="shield" />
            <b>Medha wants to {approval.action}</b>
          </div>
          {approval.detail && (
            <pre className="ask-detail">{approval.detail}</pre>
          )}
          {approval.escalated && (
            <p className="ask-note">
              This action always asks, even if you allowed it before.
            </p>
          )}
          <div className="ask-actions">
            <button
              type="button"
              className="btn-gold"
              disabled={approval.pending}
              onClick={() => onApprove(approval, true)}
            >
              {approval.pending ? "Sending…" : "Allow"}
            </button>
            <button
              type="button"
              className="btn-line"
              disabled={approval.pending}
              onClick={() => onApprove(approval, false)}
            >
              Deny
            </button>
          </div>
        </div>
      ))}
      {state.questions.map((form) => (
        <Questions
          key={form.id}
          form={form}
          onAnswer={(answers) => onAnswer(form, answers)}
        />
      ))}
      {state.verification && (
        <div
          className={`verification-line ${state.verification.ok ? "passed" : "failed"}`}
        >
          <Icon name={state.verification.ok ? "check" : "x"} />
          <span>
            {state.verification.summary ||
              (state.verification.ok
                ? "Verification passed"
                : "Verification failed")}
          </span>
        </div>
      )}
      <LiveAgents
        agents={state.agents}
        openSession={openSession}
        onOpen={onOpenAgent}
      />
      {gap && (
        <div className="live-status">
          <Spinner />
          <span>{gapLabel}</span>
        </div>
      )}
      {state.notice && <p className="quiet">{state.notice}</p>}
      {state.error && (
        <div className="error inline" role="alert">
          <b>
            {working
              ? "Couldn’t send the instruction"
              : "This turn didn’t finish"}
          </b>
          <span>{explainError(state.error) ?? state.error}</span>
        </div>
      )}
    </div>
  );

  function renderItem(item: LiveItem, index: number) {
    if (item.kind === "user") return <UserMessage message={item} />;
    if (item.kind === "reasoning")
      return showReasoning ? (
        <Reasoning text={item.text} started={item.started} ended={item.ended} />
      ) : null;
    if (item.kind === "tools") return <StepGroup steps={item.steps} live />;
    if (item.kind === "plan") return <PlanCard plan={item.plan} />;
    const streaming = working && index === state.items.length - 1;
    return item.html ? (
      <Markdown html={item.html} className={streaming ? "streaming" : ""} />
    ) : (
      <div className={`prose plain ${streaming ? "streaming" : ""}`}>
        {item.text}
      </div>
    );
  }
});

const UserMessage = memo(function UserMessage({
  message,
  pending = false,
}: {
  message: Extract<LiveItem, { kind: "user" }>;
  pending?: boolean;
}) {
  return (
    <div className={`you ${pending ? "pending" : ""}`}>
      <span className="you-mark" aria-hidden="true">
        ›
      </span>
      <p>{message.text}</p>
      <time>{clock(message.ts)}</time>
    </div>
  );
});

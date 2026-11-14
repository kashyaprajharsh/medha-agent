import { Fragment, memo, useMemo, useState } from "react";
import { SkillWriter } from "./SkillWriter";
import { Markdown } from "./Markdown";
import { Icon } from "./Icon";
import { summaryLine } from "./turnSummary";
import {
  explainError,
  planRefusal,
  queuedMessages,
  type LiveItem,
  type AccessParts,
  type Approval,
  type ApprovalChoice,
  type LiveAgent,
  type LiveState,
  type QuestionForm,
  type Step,
} from "./live";
import { LiveAgents } from "./LiveAgents";
import { Screens } from "./OutputPlate";
import { Questions } from "./Questions";
import { Spinner } from "./Spinner";
import { PlanCard, Reasoning, StepGroup } from "./Steps";
import { clock } from "./timeline";
import { TurnHead } from "./Transcript";
import { UserText } from "./UserText";
import { ProjectTrustNotice } from "./ProjectTrustNotice";
import { OutboundNotice } from "./OutboundNotice";
import { PlanLimit } from "./ChatGpt";

// A live reply is newer than anything saved, so its outputs sort last.
const LIVE = 1e12;

/** Who is asking and for what. A path request arrives as "Read access to /a/b",
 *  with the agent's name in front when one of Medha's agents is the one asking. */
/** The SKILL.md a new skill's approval proposes; the backend's preview puts it after the first blank line. */
export function skillDraft(approval: Approval): string | undefined {
  if (approval.action !== "skill.save" || !approval.detail?.startsWith("Save new skill '")) return undefined;
  const at = approval.detail.indexOf("\n\n");
  return at < 0 ? undefined : approval.detail.slice(at + 2);
}

const NO_STEPS: Step[] = [];

/** The command on its own, then each thing it may reach on its own line, the riskiest first. */
function AccessBody({ parts }: { parts: AccessParts }) {
  return (
    <>
      {(parts.command || parts.agent) && (
        <div className="ask-command">
          {parts.command && <code>{parts.command}</code>}
          {parts.workdir && <small title={parts.workdir}>in {parts.workdir}</small>}
          {parts.agent && <small>Asked by agent {parts.agent}</small>}
        </div>
      )}
      <ul className="ask-gets">
        {parts.outsideSandbox && (
          <li className="risky">
            <Icon name="alert" />
            <span>
              <b>Everything your account can reach</b>
              <small>Your files, keys and apps, for this one run</small>
            </span>
          </li>
        )}
        {parts.network && (
          <li>
            <Icon name="globe" />
            <span>
              <b>The internet</b>
              <small>Any website, for this command</small>
            </span>
          </li>
        )}
        {parts.write.map((path) => (
          <li key={`w${path}`}>
            <Icon name="folder" />
            <span>
              <b>Read and change {path}</b>
              <small>And every folder inside it</small>
            </span>
          </li>
        ))}
        {parts.read.map((path) => (
          <li key={`r${path}`}>
            <Icon name="folder" />
            <span>
              <b>Read {path}</b>
              <small>And every folder inside it</small>
            </span>
          </li>
        ))}
        {parts.hook && (
          <li>
            <Icon name="shield" />
            <span>
              <b>A hook asks you to check this</b>
              <small>{parts.hook}</small>
            </span>
          </li>
        )}
      </ul>
    </>
  );
}

function asks(approval: Approval): string {
  // A command-access card's action is already the whole question.
  if (approval.access) return approval.action;
  if (approval.kind !== "path") return `Medha wants to ${approval.action}`;
  const parts = approval.action.split(" · ");
  const request = parts.pop() ?? "";
  const who = parts.join(" · ") || "Medha";
  return `${who} wants ${request.charAt(0).toLowerCase()}${request.slice(1)}`;
}

/** Each answer says what it remembers, so the wider one is never chosen unseen. */
function answerLabel(choice: ApprovalChoice, approval: Approval): string {
  const labels: Record<ApprovalChoice, string> = {
    once: "Allow",
    always:
      approval.kind === "path"
        ? approval.path?.kind === "file"
          ? "Always allow this file"
          : approval.path?.kind === "directory"
            ? "Always allow this folder and its contents"
            : "Always allow this path"
        : "Always allow",
    folder: "Always allow this folder",
    session: "Allow for this session",
    persistent: "Always allow for this project",
    deny: "Deny",
  };
  return labels[choice];
}

type Props = {
  state: LiveState;
  showReasoning?: boolean;
  onAnswer: (
    form: QuestionForm,
    answers?: { selected: string[]; other?: string }[],
  ) => void;
  onApprove: (approval: Approval, decision: ApprovalChoice) => void;
  openSession?: string | null;
  onOpenAgent: (agent: LiveAgent) => void;
  /** Moves this session to another saved model, as the composer's model picker does. */
  onSwitchModel?: (profile: string) => void;
};

export const LiveTail = memo(function LiveTail({
  state,
  onApprove,
  onAnswer,
  showReasoning,
  openSession,
  onOpenAgent,
  onSwitchModel,
}: Props) {
  const [editing, setEditing] = useState<string>();
  const plan = state.error ? planRefusal(state.error) : undefined;
  const working =
    state.status === "running" ||
    (state.status === "starting" && state.sent.length > 0);
  const last = state.items.at(-1);
  const queued = queuedMessages(state);
  const lastTools = useMemo(
    () => state.items.reduce((found, item, index) => (item.kind === "tools" ? index : found), -1),
    [state.items],
  );
  let replying = false;
  const gap =
    working &&
    state.approvals.length === 0 &&
    state.questions.length === 0 &&
    (state.activity?.kind === "compacting" ||
      state.activity?.kind === "tool" ||
      !last ||
      last.kind === "user" ||
      queued.length > 0 ||
      (last.kind === "tools" &&
        !last.steps.some((step) => step.status === "running")) ||
      (last.kind === "reasoning" && last.ended !== undefined));
  const gapLabel =
    state.status === "starting"
      ? "Starting Medha…"
      : state.activity?.kind === "compacting"
        ? "Condensing earlier messages to fit the model’s context…"
        : state.activity?.kind === "tool" && (state.activity.tool === "write" || state.activity.tool === "edit")
          ? `Writing ${state.activity.target?.split(/[\\/]/).at(-1) || "a file"}`
        : last && last.kind !== "reasoning"
          ? "Working"
          : "Thinking";
  return (
    <div className="live-tail" aria-live="polite">
      {state.items.map((item, index) => {
        const first = item.kind !== "user" && !replying;
        if (item.kind === "user") replying = false;
        else if (first) replying = true;
        return (
          <Fragment key={item.id}>
            {first && <TurnHead />}
            {renderItem(item, index)}
          </Fragment>
        );
      })}
      {/* A screen still being written sits with the newest work, until its result
          puts it under the step that made it. */}
      <Screens steps={NO_STEPS} writing={state.writing} at={LIVE} fresh />
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
            <b>{asks(approval)}</b>
          </div>
          {approval.access ? (
            <AccessBody parts={approval.access} />
          ) : (
            approval.detail && <pre className="ask-detail">{approval.detail}</pre>
          )}
          {approval.access?.untrusted ? (
            <p className="ask-note ask-why">
              It read web content earlier in this chat, so this is asked every time and can't be remembered.
            </p>
          ) : (
            approval.escalated && (
              <p className={approval.access ? "ask-note ask-why" : "ask-note"}>
                This action always asks, even if you allowed it before.
              </p>
            )
          )}
          {approval.folder && (
            <p className="ask-note">Its folder: {approval.folder}</p>
          )}
          <div className="ask-actions">
            {approval.choices.map((choice) => (
              <button
                type="button"
                key={choice}
                className={
                  choice === "once"
                    ? "btn-gold"
                    : choice === "deny"
                      ? "btn-line ask-deny"
                      : "btn-line"
                }
                disabled={approval.pending}
                onClick={() => onApprove(approval, choice)}
              >
                {choice === "once" && approval.pending
                  ? "Sending…"
                  : answerLabel(choice, approval)}
              </button>
            ))}
            {skillDraft(approval) && (
              <button
                type="button"
                className="btn-line"
                disabled={approval.pending}
                onClick={() => {
                  // The person saves it from the editor, so the model's own save is declined.
                  setEditing(skillDraft(approval));
                  onApprove(approval, "deny");
                }}
              >
                Open in the editor
              </button>
            )}
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
      {state.summary && <p className="quiet turn-summary">{summaryLine(state.summary)}</p>}
      {state.projectTrust && <ProjectTrustNotice key={`${state.sessionId}:${state.projectTrust.root}:${state.projectTrust.fingerprint}`} review={state.projectTrust} session={state.sessionId} />}
      {state.outbound && <OutboundNotice key={`${state.sessionId}:${state.outbound.id}`} review={state.outbound} />}
      {state.mutations?.map(review => <OutboundNotice kind="protected" key={`${state.sessionId}:${review.id}`} review={review} />)}
      {plan && (
        <PlanLimit
          code={plan}
          others={(state.settings?.profiles ?? []).filter((profile) => !profile.chatgpt_plan).map((profile) => profile.name)}
          onSwitch={(profile) => onSwitchModel?.(profile)}
        />
      )}
      {state.error && !plan && (
        <div className="error inline" role="alert">
          <b>
            {working
              ? "Couldn’t send the instruction"
              : "This turn didn’t finish"}
          </b>
          <span>{explainError(state.error) ?? state.error}</span>
        </div>
      )}
      {editing !== undefined && (
        <SkillWriter
          disabled={false}
          initial={[{ path: "SKILL.md", text: editing }]}
          onClose={() => setEditing(undefined)}
          onSaved={() => window.dispatchEvent(new CustomEvent("medha-preferences"))}
        />
      )}
    </div>
  );

  function renderItem(item: LiveItem, index: number) {
    if (item.kind === "user") return <UserMessage message={item} />;
    if (item.kind === "reasoning")
      return (
        <Reasoning
          text={item.text}
          started={item.started}
          ended={item.ended}
          expanded={showReasoning}
        />
      );
    // What a step made sits under it, as in a finished chat; only the newest group opens its screen.
    if (item.kind === "tools")
      return (
        <>
          <StepGroup steps={item.steps} live />
          <Screens steps={item.steps} at={LIVE + index} fresh={index === lastTools} />
        </>
      );
    if (item.kind === "plan") return <PlanCard plan={item.plan} />;
    // Once the model has moved on to a tool call, this text is finished: the
    // status line below shows the work, not a caret after the last word.
    const streaming = working && index === state.items.length - 1 && state.activity?.kind !== "tool";
    return item.html ? (
      <Markdown html={item.html} streaming={streaming} scope={item.sourceEvent ?? `live-${index}`} at={LIVE + index} fresh />
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
      <UserText text={message.text} images={message.images} about={message.about} />
      <time>{clock(message.ts)}</time>
    </div>
  );
});

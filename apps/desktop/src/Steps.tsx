import { useWorkspace } from "./Workspace";
import { memo, useEffect, useState, type ReactNode } from "react";
import { Icon, type IconName } from "./Icon";
import type { Plan, Step } from "./live";
import { Diff } from "./GitChanges";
import { SourceCode } from "./Markdown";
import { Spinner } from "./Spinner";
import { stepVerb } from "./timeline";

function stepKind(step: Step): { kind: string; icon: IconName; label: string } {
  const verb = stepVerb(step);
  if (/^(Read|Listed|Found files|Outlined)/.test(verb))
    return { kind: "read", icon: "file", label: "Read" };
  if (/^(Search|Found references|Fetched)/.test(verb))
    return { kind: "search", icon: "search", label: "Search" };
  if (/^(Wrote|Edited|Prepare edit)/.test(verb))
    return { kind: "edit", icon: "edit", label: "Edit" };
  if (/^Ran/.test(verb)) return { kind: "run", icon: "term", label: "Command" };
  if (/agent|work to|Steered|Stopped/.test(verb))
    return { kind: "agent", icon: "fork", label: "Agent" };
  return { kind: "other", icon: "tool", label: verb };
}

function seconds(value: number) {
  return value < 10
    ? `${Math.max(0.1, Math.round(value * 10) / 10)}s`
    : `${Math.round(value)}s`;
}

// Closed details must not mount/highlight large tool output or update hidden
// reasoning. Keep previously opened content mounted for the fold animation.
const FoldContent = memo(
  function FoldContent({
    open,
    children,
  }: {
    open: boolean;
    children: ReactNode;
    freeze?: boolean;
  }) {
    const [visited, setVisited] = useState(open);
    if (open && !visited) setVisited(true);
    return open || visited ? children : null;
  },
  (_previous, next) => next.freeze !== false && !next.open,
);

export function useNow(active: boolean) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => setNow(Date.now() / 1000), 250);
    return () => clearInterval(timer);
  }, [active]);
  return now;
}

export const StepGroup = memo(function StepGroup({
  steps,
  live = false,
}: {
  steps: Step[];
  live?: boolean;
}) {
  const running = steps.some((step) => step.status === "running");
  const [open, setOpen] = useState(live);
  const now = useNow(running);
  const starts = steps
    .map((step) => step.started)
    .filter((value): value is number => value !== undefined);
  const ends = steps
    .map(
      (step) => step.ended ?? (step.status === "running" ? now : step.started),
    )
    .filter((value): value is number => value !== undefined);
  const span =
    starts.length && ends.length
      ? Math.max(0, Math.max(...ends) - Math.min(...starts))
      : undefined;
  const failed = steps.filter(
    (step) => step.status === "failed" || step.status === "denied",
  ).length;
  return (
    <section className={`work ${running ? "is-running" : ""}`}>
      <button
        type="button"
        className="work-head"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <Icon name="chev" className="icon chev" />
        <b>
          {running
            ? "Working"
            : span !== undefined
              ? `Worked for ${seconds(span)}`
              : "Worked"}
        </b>
        <span className="work-count">
          {steps.length} {steps.length === 1 ? "step" : "steps"}
        </span>
        <span className="work-dots" aria-hidden="true">
          {[
            ...new Map(
              steps.map((step) => {
                const type = stepKind(step);
                return [type.kind, type] as const;
              }),
            ).values(),
          ].map((type) => (
            <i
              key={type.kind}
              className={`kind-${type.kind}`}
              title={type.label}
            />
          ))}
        </span>
        {failed > 0 && <span className="work-failed">{failed} failed</span>}
      </button>
      <div className={`fold ${open ? "open" : ""}`}>
        <div>
          <FoldContent open={open} freeze={false}>
            <ol className="steps">
              {steps.map((step) => (
                <StepRow key={step.id} step={step} now={now} />
              ))}
            </ol>
          </FoldContent>
        </div>
      </div>
    </section>
  );
});

const StepRow = memo(
  function StepRow({ step, now }: { step: Step; now: number }) {
    const context = useWorkspace();
    const type = stepKind(step);
    function openFile() {
      if (step.filePath)
        window.dispatchEvent(
          new CustomEvent("medha-file", {
            detail: { workspace: context.workspace.id, path: step.filePath },
          }),
        );
    }
    function showChange() {
      if (step.filePath)
        window.dispatchEvent(
          new CustomEvent("medha-change", {
            detail: { workspace: context.workspace.id, path: step.filePath },
          }),
        );
    }
    const changed =
      step.status === "ok" && (step.verb === "Wrote" || step.verb === "Edited");
    const [open, setOpen] = useState(false);
    const bad = step.status === "failed" || step.status === "denied";
    const took =
      step.started !== undefined
        ? (step.ended ?? (step.status === "running" ? now : undefined))
        : undefined;
    const expandable = Boolean(step.detail || step.input || step.output);
    return (
      <li
        className={`step ${bad ? "failed" : ""} ${step.status === "running" ? "running" : ""}`}
      >
        <div className="step-row">
          <button
            type="button"
            className="step-main"
            aria-expanded={expandable ? open : undefined}
            disabled={!expandable && !step.filePath}
            onClick={() => (expandable ? setOpen(!open) : openFile())}
          >
            <span className={`step-icon kind-${type.kind}`} title={step.status}>
              {step.status === "running" ? (
                <Spinner />
              ) : (
                <Icon
                  name={bad || step.status === "stopped" ? "x" : type.icon}
                />
              )}
            </span>
            <span className="step-label">{stepVerb(step)}</span>
          </button>
          {step.target &&
            (step.filePath ? (
              <button
                className="step-target file-link"
                type="button"
                title={`Open ${step.filePath}`}
                onClick={openFile}
              >
                <code>{step.target}</code>
              </button>
            ) : (
              <code className="step-target">{step.target}</code>
            ))}
          <span className={`step-meta ${bad ? "bad" : ""}`}>
            {step.status === "denied"
              ? "Denied"
              : step.status === "failed"
                ? "Failed"
                : step.status === "stopped"
                  ? "Stopped"
                  : (step.summary ?? "")}
          </span>
          <span className="step-dur">
            {took !== undefined && step.started !== undefined
              ? seconds(took - step.started)
              : ""}
          </span>
          {expandable && (
            <button
              className="icon-btn sm"
              type="button"
              aria-label={open ? "Hide step details" : "Show step details"}
              aria-expanded={open}
              onClick={() => setOpen(!open)}
            >
              <Icon name="chev" className="icon chev" />
            </button>
          )}
        </div>
        {expandable && (
          <div className={`fold ${open ? "open" : ""}`}>
            <div>
              <FoldContent open={open}>
                <div className="step-inner">
                  {step.filePath && (
                    <div className="step-actions">
                      <button className="btn-line" onClick={openFile}>
                        <Icon name="file" />
                        Open file
                      </button>
                      {changed && (
                        <button className="btn-line" onClick={showChange}>
                          <Icon name="branch" />
                          Show in Changes
                        </button>
                      )}
                    </div>
                  )}
                  {step.input && (
                    <div>
                      <span className="io-label">
                        {step.inputLabel ?? "Input"}
                      </span>
                      <pre
                        className={`io ${step.inputLabel === "Command" ? "" : "prose"}`}
                      >
                        {step.input}
                      </pre>
                    </div>
                  )}
                  {step.detail && (
                    <div>
                      <span className="io-label">
                        {step.status === "denied"
                          ? "Why it was stopped"
                          : "Error"}
                      </span>
                      <pre className="io failed">{step.detail}</pre>
                    </div>
                  )}
                  {step.output && !step.detail && (
                    <div>
                      <span className="io-label">Output</span>
                      {step.output.startsWith("--- ") ||
                      step.output.startsWith("diff --git ") ? (
                        <Diff text={step.output} />
                      ) : step.filePath &&
                        ["Read", "Wrote", "Edited"].includes(stepVerb(step)) ? (
                        <SourceCode
                          text={step.output}
                          language={
                            step.filePath.endsWith(".py") ? "python" : undefined
                          }
                        />
                      ) : (
                        <pre className="io result">{step.output}</pre>
                      )}
                    </div>
                  )}
                  {step.status === "running" && !step.output && (
                    <p className="io-waiting">Waiting for the result…</p>
                  )}
                </div>
              </FoldContent>
            </div>
          </div>
        )}
      </li>
    );
  },
  (previous, next) =>
    previous.step === next.step &&
    (next.step.status !== "running" || previous.now === next.now),
);

const DONE = new Set(["completed", "done"]);
const ACTIVE = new Set(["in_progress", "active", "running"]);

export const PlanCard = memo(function PlanCard({ plan }: { plan: Plan }) {
  const done = plan.steps.filter((step) => DONE.has(step.status)).length;
  const total = plan.steps.length;
  return (
    <section className="plan" aria-label="Plan">
      <header className="plan-head">
        <Icon name="list" />
        <b>Plan</b>
        <span className="plan-count">
          {done} of {total} done
        </span>
        <span className="plan-bar" aria-hidden="true">
          <i style={{ width: `${total ? (done / total) * 100 : 0}%` }} />
        </span>
      </header>
      {plan.explanation && <p className="plan-why">{plan.explanation}</p>}
      <ol className="plan-steps">
        {plan.steps.map((step, index) => {
          const state = DONE.has(step.status)
            ? "done"
            : ACTIVE.has(step.status)
              ? "active"
              : "pending";
          return (
            <li key={index} className={`plan-step ${state}`}>
              <span
                className="plan-mark"
                aria-label={
                  state === "done"
                    ? "Done"
                    : state === "active"
                      ? "In progress"
                      : "Pending"
                }
              >
                {state === "done" ? (
                  <Icon name="check" />
                ) : state === "active" ? (
                  <Spinner />
                ) : (
                  <span className="plan-dot" />
                )}
              </span>
              <span>{step.title}</span>
            </li>
          );
        })}
      </ol>
    </section>
  );
});

export const Reasoning = memo(function Reasoning({
  text,
  started,
  ended,
  durationMs,
}: {
  text: string;
  started?: number;
  ended?: number;
  durationMs?: number;
}) {
  const thinking = started !== undefined && ended === undefined;
  const [open, setOpen] = useState(false);
  const now = useNow(thinking);
  const span =
    durationMs !== undefined
      ? durationMs / 1000
      : started !== undefined
        ? (ended ?? now) - started
        : undefined;
  return (
    <section className={`thought ${thinking ? "thinking" : ""}`}>
      <button
        type="button"
        className="thought-head"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        {thinking ? <Spinner /> : <Icon name="chev" className="icon chev" />}
        <span className="thought-label">
          {thinking
            ? "Thinking"
            : span !== undefined
              ? `Thought for ${seconds(span)}`
              : "Thought"}
        </span>
        {thinking && span !== undefined && (
          <span className="thought-time">{Math.floor(span)}s</span>
        )}
      </button>
      <div className={`fold ${open ? "open" : ""}`}>
        <div>
          <FoldContent open={open}>
            <p className="thought-text">{text}</p>
          </FoldContent>
        </div>
      </div>
    </section>
  );
});

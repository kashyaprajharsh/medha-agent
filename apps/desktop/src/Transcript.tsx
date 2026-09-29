import { memo } from "react";
import { Markdown } from "./Markdown";
import { Icon } from "./Icon";
import { PlanCard, Reasoning, StepGroup } from "./Steps";
import type { Block, SubagentRun } from "./timeline";
import { clock, doingLine, duration, outcomeLabel } from "./timeline";
import type { LiveAgent } from "./live";
import { UserText } from "./UserText";

type Props = {
  blocks: Block[];
  onRewind?: (id: string) => void;
  showReasoning?: boolean;
  openRun?: string | null;
  onOpenRun?: (run: SubagentRun) => void;
  hideHandoff?: boolean;
  liveAgents?: LiveAgent[];
};

const MEDHA_SPEAKS = new Set<Block["kind"]>([
  "assistant",
  "reasoning",
  "plan",
  "tools",
  "subagents",
  "verification",
]);

export function TurnHead({ ts }: { ts?: number }) {
  return (
    <div className="turn-head">
      <span className="turn-mark" aria-hidden="true">
        ◆
      </span>
      Medha
      {ts !== undefined && <time>{clock(ts)}</time>}
    </div>
  );
}

export const Transcript = memo(function Transcript({
  blocks,
  onRewind,
  openRun,
  onOpenRun,
  hideHandoff,
  liveAgents,
  showReasoning = true,
}: Props) {
  const task = blocks.find((block) => block.kind === "handoff")?.id;
  let replying = false;
  return (
    <>
      {blocks.map((block) => {
        const speaksFirst = !replying && MEDHA_SPEAKS.has(block.kind);
        replying =
          block.kind === "user" || block.kind === "handoff"
            ? false
            : replying || speaksFirst;
        return (
          <div className={speaksFirst ? "turn" : "turn-part"} key={block.id}>
            {speaksFirst && <TurnHead ts={block.ts} />}
            {render(block)}
          </div>
        );
      })}
    </>
  );

  function render(block: Block) {
    switch (block.kind) {
      case "user":
        return (
          <div className="you">
            <span className="you-mark" aria-hidden="true">
              ›
            </span>
            <UserText text={block.text} />
            <time>{clock(block.ts)}</time>
            {onRewind && (
              <button
                className="message-rewind"
                title="Rewind to before this message"
                aria-label="Rewind to before this message"
                onClick={() => onRewind(block.id)}
              >
                <Icon name="rewind" />
              </button>
            )}
          </div>
        );
      case "handoff":
        if (block.id === task) {
          return hideHandoff ? null : (
            <details className="handoff">
              <summary>
                <Icon name="fork" />
                Task handed over by Medha
              </summary>
              <p>{block.text}</p>
            </details>
          );
        }
        return (
          <div className="from-parent">
            <span className="from-label">
              <Icon name="fork" />
              From Medha
            </span>
            <p>{block.text}</p>
            <time>{clock(block.ts)}</time>
          </div>
        );
      case "assistant":
        // The backend escapes all model text and keeps only web and mail links.
        return block.html ? (
          <Markdown html={block.html} />
        ) : (
          <div className="prose plain">{block.text}</div>
        );
      case "reasoning":
        return (
          <Reasoning
            text={block.text}
            durationMs={block.durationMs}
            expanded={showReasoning}
          />
        );
      case "plan":
        return <PlanCard plan={block.plan} />;
      case "verification":
        return (
          <details className={`verification-line ${block.ok ? "passed" : "failed"}`}>
            <summary>
              <Icon name={block.ok ? "check" : "x"} />
              <span>{block.summary || (block.ok ? "Verification passed" : "Verification failed")}</span>
            </summary>
            {block.output && <pre className="io result">{block.output}</pre>}
          </details>
        );
      case "tools":
        return <StepGroup steps={block.steps} />;
      case "subagents":
        return (
          <SubagentCard
            runs={block.runs}
            openRun={openRun}
            onOpen={onOpenRun}
            live={liveAgents}
          />
        );
    }
  }
});

type CardProps = {
  runs: SubagentRun[];
  openRun?: string | null;
  onOpen?: (run: SubagentRun) => void;
  live?: LiveAgent[];
};

function SubagentCard({ runs: recorded, openRun, onOpen, live }: CardProps) {
  const runs = recorded.map((run) => {
    const agent = live?.find((candidate) => candidate.session === run.childId);
    return agent
      ? { ...run, status: agent.status, doing: doingLine(agent) }
      : { ...run, doing: undefined };
  });
  const done = runs.filter((run) => run.status === "completed").length;
  return (
    <section className="agents">
      <header className="agents-head">
        <Icon name="fork" />
        <b>
          Started {runs.length} {runs.length === 1 ? "sub-agent" : "sub-agents"}
        </b>
        <span>
          {done === runs.length ? "All done" : `${done} of ${runs.length} done`}
        </span>
      </header>
      <ul>
        {runs.map((run) => (
          <li key={run.id}>
            <button
              type="button"
              className={`agent-row ${openRun === run.childId ? "active" : ""}`}
              onClick={() => onOpen?.(run)}
              disabled={!run.childId || !onOpen}
            >
              <span className={`agent-dot ${run.status ?? "none"}`} />
              <span className="agent-main">
                <b>{run.name}</b>
                <span>{run.objective || "No objective recorded"}</span>
              </span>
              <span className="agent-meta">
                {run.status === "running"
                  ? run.doing
                  : outcomeLabel(run.status)}
                {run.status !== "running" && run.durationMs !== undefined
                  ? `, ${duration(run.durationMs)}`
                  : ""}
              </span>
              <Icon name="chev" className="icon chev" />
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

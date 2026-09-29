import { FilesBrowser } from "./FilesBrowser";
import { GitChanges } from "./GitChanges";
import { SessionChanges } from "./SessionChanges";
import { BackgroundTasks } from "./Tasks";
import { useWorkspaceApi } from "./Workspace";
import { useEffect, useMemo, useState } from "react";
import { type Patch, type SessionSettings } from "./api";
import { Icon } from "./Icon";
import type { LiveState } from "./live";
import { SubagentPanel } from "./SubagentPanel";
import {
  doingLine,
  outcomeLabel,
  tokens,
  type Block,
  type SubagentRun,
} from "./timeline";

export type SurfaceTab =
  | "agents"
  | "changes"
  | "files"
  | "activity"
  | "context";
type Props = {
  onFile: (path: string) => void;
  tab: SurfaceTab;
  onTab: (tab: SurfaceTab) => void;
  onClose: () => void;
  state?: LiveState;
  settings?: SessionSettings;
  blocks: Block[];
  run: SubagentRun | null;
  onAgent: (run: SubagentRun | null) => void;
  showReasoning: boolean;
  sessionKey: string | null;
  sessionId: string | null;
  focusChange?: { path: string; at: number };
  ensureOpen: () => Promise<void>;
  locked: boolean;
};

export function WorkSurface(props: Props) {
  const api = useWorkspaceApi();
  const [changeView, setChangeView] = useState("session");
  const [sessionFiles, setSessionFiles] = useState(0);
  const [patches, setPatches] = useState<Patch[]>([]);
  const [selectedPatch, setSelectedPatch] = useState<string>();
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const [refresh, setRefresh] = useState(0);
  const [copied, setCopied] = useState(false);
  const recorded = useMemo(
    () =>
      props.blocks.flatMap((block) =>
        block.kind === "subagents" ? block.runs : [],
      ),
    [props.blocks],
  );
  const agents = props.state?.agents ?? [];
  const runs = [
    ...recorded.filter(
      (run) => !agents.some((agent) => agent.session === run.childId),
    ),
    ...agents.map((agent) => ({
      id: agent.session,
      childId: agent.session,
      name: agent.name,
      objective: agent.objective,
      status: agent.status,
    })),
  ];
  const unique = runs.filter(
    (run, index) =>
      runs.findIndex((candidate) => candidate.childId === run.childId) ===
      index,
  );
  const running = agents.some((agent) => agent.status === "running");
  const rosterVersion = agents
    .map((agent) => `${agent.session}:${agent.status}`)
    .join("|");

  useEffect(() => {
    setSessionFiles(0);
    setPatches([]);
    setSelectedPatch(undefined);
    setError(undefined);
    setNotice(undefined);
  }, [props.sessionKey, props.sessionId]);
  useEffect(() => {
    if (
      props.tab !== "changes" ||
      changeView !== "patches" ||
      !props.sessionKey
    )
      return;
    let active = true;
    let reading = false;
    const read = async () => {
      if (reading) return;
      reading = true;
      setLoading(true);
      try {
        await props.ensureOpen();
        const result = await api.liveCall(props.sessionKey!, "patch.list");
        if (!active) return;
        const rows = (result.patches ?? []) as Patch[];
        setPatches(rows);
        setError(undefined);
        setSelectedPatch((selected) =>
          rows.some((patch) => patch.id === selected) ? selected : rows[0]?.id,
        );
      } catch (cause) {
        if (active) setError(String(cause));
      } finally {
        reading = false;
        if (active) setLoading(false);
      }
    };
    void read();
    const timer = running ? setInterval(() => void read(), 3000) : undefined;
    return () => {
      active = false;
      if (timer) clearInterval(timer);
    };
  }, [
    props.tab,
    changeView,
    props.sessionKey,
    rosterVersion,
    props.state?.settled,
    refresh,
  ]);

  async function apply(patch: Patch) {
    if (!props.sessionKey) return;
    setBusy(true);
    setError(undefined);
    setNotice(undefined);
    try {
      await props.ensureOpen();
      await api.liveCall(props.sessionKey, "patch.apply", {
        patch_id: patch.id,
      });
      setNotice(
        `Applied ${patch.files.length} ${patch.files.length === 1 ? "file" : "files"} from ${patch.agent}.`,
      );
      setRefresh((value) => value + 1);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }
  const patch = patches.find((patch) => patch.id === selectedPatch);
  const activity = props.blocks.flatMap((block) =>
    block.kind === "tools" ? block.steps : [],
  );
  const liveSteps =
    props.state?.items.flatMap((item) =>
      item.kind === "tools" ? item.steps : [],
    ) ?? [];
  const steps = [...activity, ...liveSteps];
  const liveWrites = liveSteps.filter(
    (step) =>
      step.status === "ok" && (step.verb === "Wrote" || step.verb === "Edited"),
  ).length;
  useEffect(() => {
    if (props.focusChange) setChangeView("session");
  }, [props.focusChange]);
  const latestPlan = [...props.blocks]
    .reverse()
    .find((block) => block.kind === "plan");
  const latestLivePlan = [...(props.state?.items ?? [])]
    .reverse()
    .find((item) => item.kind === "plan");
  const plan =
    latestLivePlan?.kind === "plan"
      ? latestLivePlan.plan
      : latestPlan?.kind === "plan"
        ? latestPlan.plan
        : undefined;

  return (
    <aside className="panel work-surface" aria-label="Work surface">
      <header className="surface-head">
        <b>Work surface</b>
        <button
          type="button"
          className="icon-btn"
          onClick={props.onClose}
          title="Close · ⌘."
          aria-label="Close work surface"
        >
          <Icon name="x" />
        </button>
      </header>
      <div
        className="surface-tabs"
        role="tablist"
        aria-label="Work surface views"
      >
        {(
          ["agents", "changes", "files", "activity", "context"] as SurfaceTab[]
        ).map((tab) => (
          <button
            type="button"
            role="tab"
            key={tab}
            id={`tab-${tab}`}
            aria-controls={`surface-${tab}`}
            aria-selected={props.tab === tab}
            onClick={() => props.onTab(tab)}
            onKeyDown={(event) => {
              const tabs: SurfaceTab[] = [
                "agents",
                "changes",
                "files",
                "activity",
                "context",
              ];
              if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
                event.preventDefault();
                const next =
                  tabs[
                    (tabs.indexOf(tab) + (event.key === "ArrowRight" ? 1 : 4)) %
                      5
                  ];
                props.onTab(next);
                document.getElementById(`tab-${next}`)?.focus();
              }
            }}
            tabIndex={props.tab === tab ? 0 : -1}
          >
            {tab[0].toUpperCase() + tab.slice(1)}
            {tab === "changes" && sessionFiles > 0 && (
              <span>{sessionFiles}</span>
            )}
          </button>
        ))}
      </div>
      <div
        className={`surface-content ${props.tab === "agents" && props.run ? "has-agent" : ""}`}
        role="tabpanel"
        id={`surface-${props.tab}`}
        aria-labelledby={`tab-${props.tab}`}
      >
        {props.tab === "files" && (
          <FilesBrowser
            onFile={props.onFile}
            version={props.sessionKey || ""}
          />
        )}
        {props.tab === "changes" && (
          <>
            <div className="segmented">
              <button
                aria-pressed={changeView === "session"}
                onClick={() => setChangeView("session")}
              >
                This session
              </button>
              <button
                aria-pressed={changeView === "working"}
                onClick={() => setChangeView("working")}
              >
                Git changes
              </button>
              <button
                aria-pressed={changeView === "patches"}
                onClick={() => setChangeView("patches")}
              >
                Agent patches
              </button>
            </div>
            {changeView === "session" && (
              <SessionChanges
                key={props.sessionId || "draft"}
                sessionId={props.sessionId}
                version={`${props.state?.settled}:${liveWrites}`}
                focus={props.focusChange}
                onFile={props.onFile}
                onCount={setSessionFiles}
              />
            )}
            {changeView === "working" && (
              <GitChanges
                onFile={props.onFile}
                version={`${props.sessionKey}:${props.state?.settled}`}
              />
            )}
          </>
        )}
        {props.tab === "agents" &&
          (props.run ? (
            <SubagentPanel
              run={props.run}
              live={agents.find(
                (agent) => agent.session === props.run?.childId,
              )}
              onClose={() => props.onAgent(null)}
              showReasoning={props.showReasoning}
              onAction={async (action, text) => {
                if (!props.sessionKey)
                  throw new Error("Select a session first.");
                await props.ensureOpen();
                await api.liveCall(props.sessionKey, "agent.control", {
                  agent: props.run!.childId,
                  action,
                  text,
                });
              }}
            />
          ) : (
            <div className="surface-body">
              {!unique.length && (
                <Empty
                  icon="fork"
                  title="No delegated work yet"
                  text="Agents appear here when Medha delegates a task."
                />
              )}
              {unique.map((run) => {
                const live = agents.find(
                  (agent) => agent.session === run.childId,
                );
                return (
                  <button
                    type="button"
                    className="surface-agent"
                    key={run.childId}
                    onClick={() => props.onAgent(run)}
                  >
                    <span className={`agent-dot ${run.status ?? "none"}`} />
                    <span>
                      <b>{live?.path?.replace(/^\//, "") || run.name}</b>
                      <small>
                        {live ? doingLine(live) : outcomeLabel(run.status)}
                      </small>
                      <p>{run.objective}</p>
                      {live && (
                        <small>
                          {live.write ? "Writer" : "Read-only"}
                          {live.toolCalls !== undefined
                            ? ` · ${live.toolCalls} tools`
                            : ""}
                          {live.tokens !== undefined
                            ? ` · ${tokens(live.tokens)}`
                            : ""}
                        </small>
                      )}
                    </span>
                    <Icon name="chev" />
                  </button>
                );
              })}
            </div>
          ))}
        {props.tab === "changes" && changeView === "patches" && (
          <div className="surface-body">
            {loading && !patches.length && (
              <p className="quiet" role="status">
                Reading pending changes…
              </p>
            )}
            {!loading && !patches.length && !error && (
              <Empty
                icon="branch"
                title="No pending agent patches"
                text="Writing agents work in isolation. Their proposed changes appear here for review."
              />
            )}
            {notice && (
              <p className="surface-notice" role="status">
                <Icon name="check" />
                {notice}
              </p>
            )}
            {error && (
              <p className="surface-error" role="alert">
                {error}
                <button
                  type="button"
                  className="link-btn"
                  onClick={() => setRefresh((value) => value + 1)}
                >
                  Refresh
                </button>
              </p>
            )}
            {patches.length > 1 && (
              <div className="patch-selector">
                {patches.map((row) => (
                  <button
                    type="button"
                    key={row.id}
                    aria-pressed={row.id === selectedPatch}
                    onClick={() => setSelectedPatch(row.id)}
                  >
                    {row.agent}
                    <small>{row.files.length} files</small>
                  </button>
                ))}
              </div>
            )}
            {patch && (
              <>
                <div className="patch-title">
                  <b>{patch.agent}</b>
                  <span>
                    {patch.files.length}{" "}
                    {patch.files.length === 1 ? "file" : "files"} proposed
                  </span>
                  <button
                    type="button"
                    className="icon-btn"
                    title="Copy diff"
                    aria-label="Copy diff"
                    onClick={() => {
                      void navigator.clipboard
                        .writeText(patch.diff)
                        .then(() => {
                          setCopied(true);
                          setTimeout(() => setCopied(false), 2000);
                        })
                        .catch((cause) => setError(String(cause)));
                    }}
                  >
                    <Icon name={copied ? "check" : "copy"} />
                  </button>
                </div>
                <div className="patch-files">
                  {patch.files.map((file) => (
                    <span key={file}>
                      <Icon name="file" />
                      <code>{file}</code>
                    </span>
                  ))}
                </div>
                <div
                  className={`patch-verification ${patch.verification?.passed === false ? "failed" : ""}`}
                >
                  <Icon
                    name={
                      patch.verification
                        ? patch.verification.passed
                          ? "check"
                          : "x"
                        : "shield"
                    }
                  />
                  <span>
                    {patch.verification
                      ? `${patch.verification.passed ? "Passed" : "Failed"}: ${patch.verification.command}`
                      : "No verification recorded"}
                  </span>
                </div>
                {patch.verification?.output && (
                  <details className="verification-output">
                    <summary>Check output</summary>
                    <pre>{patch.verification.output}</pre>
                  </details>
                )}
                <Diff text={patch.diff} />
                <div className="patch-actions">
                  <span>
                    {props.locked
                      ? "Finish active work before applying."
                      : "Applies to your working tree."}
                  </span>
                  <button
                    type="button"
                    className="btn-gold"
                    disabled={
                      busy ||
                      props.locked ||
                      patch.verification?.passed === false
                    }
                    onClick={() => void apply(patch)}
                  >
                    {busy ? "Applying…" : "Apply patch"}
                  </button>
                </div>
              </>
            )}
          </div>
        )}
        {props.tab === "activity" && (
          <div className="surface-body">
            <BackgroundTasks
              sessionKey={props.sessionKey}
              live={Boolean(props.state && props.state.status !== "exited" && props.state.status !== "starting")}
              version={`${props.state?.settled}:${liveSteps.length}`}
            />
            {!steps.length && (
              <Empty
                icon="term"
                title="No tool activity yet"
                text="Commands and tool results appear here as Medha works."
              />
            )}
            {steps.map((step, index) => (
              <details className="activity-step" key={`${step.id}-${index}`}>
                <summary>
                  <Icon
                    name={
                      step.status === "ok"
                        ? "check"
                        : step.status === "running"
                          ? "refresh"
                          : "x"
                    }
                  />
                  <span>
                    {step.verb || step.tool}{" "}
                    {step.target && <code>{step.target}</code>}
                  </span>
                  <small>
                    {step.status === "running"
                      ? "Working"
                      : step.summary || step.status}
                  </small>
                </summary>
                {step.input && <pre>{step.input}</pre>}
                {step.output && <pre>{step.output}</pre>}
                {step.detail && (
                  <pre className="surface-error">{step.detail}</pre>
                )}
                {!step.input && !step.output && !step.detail && (
                  <p className="quiet">
                    {step.status === "running"
                      ? "Waiting for the result…"
                      : "No additional output."}
                  </p>
                )}
              </details>
            ))}
          </div>
        )}
        {props.tab === "context" && (
          <div className="surface-body context-details">
            <div className="context-heading">
              <Icon name="list" />
              <div>
                <b>Session context</b>
                <span>
                  {props.state?.contextPercent !== undefined
                    ? `${props.state.contextPercent}% of the input budget used${props.state.contextPressure?.quality === "local_estimate" ? " (estimated)" : ""}`
                    : "Context usage appears after the next model request."}
                </span>
              </div>
            </div>
            {props.state?.contextPercent !== undefined && (
              <div className="context-meter">
                <i
                  style={{
                    width: `${Math.min(100, props.state.contextPercent)}%`,
                  }}
                />
              </div>
            )}
            <dl>
              <dt>Model</dt>
              <dd>
                {props.settings?.model ??
                  props.state?.model ??
                  "Configured default"}
              </dd>
              <dt>Context window</dt>
              <dd>
                {props.settings?.context_limit
                  ? `${props.settings.context_limit.toLocaleString()} tokens`
                  : "Not reported"}
              </dd>
              {props.state?.contextPressure?.usable !== undefined && (
                <>
                  <dt>Usable input budget</dt>
                  <dd>
                    {props.state.contextPressure.usable.toLocaleString()} tokens
                  </dd>
                  <dt>Current input</dt>
                  <dd>
                    {props.state.contextPressure.input.toLocaleString()} tokens
                  </dd>
                </>
              )}
              {props.state?.compaction && (
                <>
                  <dt>Last compaction method</dt>
                  <dd>
                    {props.state.compaction.summarized === false
                      ? "Tool-output pruning"
                      : props.state.compaction.summary?.includes(
                            "[MEDHA extractive summary",
                          )
                        ? "Local fallback summary"
                        : props.state.compaction.summary
                          ? "Model summary"
                          : "Not reported"}
                  </dd>
                </>
              )}
              <dt>Execution mode</dt>
              <dd>{props.settings?.mode ?? "Configured default"}</dd>
              <dt>Reasoning</dt>
              <dd>
                {props.settings?.reasoning ?? "Auto"}
                {props.settings?.effort && props.settings.effort !== "auto"
                  ? ` · ${props.settings.effort}`
                  : ""}
              </dd>
              {props.state?.usage && (
                <>
                  <dt>Latest request tokens</dt>
                  <dd>{props.state.usage.total.toLocaleString()}</dd>
                  <dt>Prompt tokens</dt>
                  <dd>{props.state.usage.prompt.toLocaleString()}</dd>
                  <dt>Cached prompt tokens</dt>
                  <dd>
                    {props.state.usage.cached === undefined
                      ? "Not reported"
                      : props.state.usage.cached.toLocaleString()}
                  </dd>
                </>
              )}
            </dl>
            {props.state?.compaction && (
              <details className="context-compaction">
                <summary>
                  Last compaction ·{" "}
                  {props.state.compaction.before.toLocaleString()} →{" "}
                  {props.state.compaction.after.toLocaleString()} tokens
                </summary>
                <p>
                  {props.state.compaction.summary ||
                    "The runtime reduced conversation context for the next request."}
                </p>
              </details>
            )}
            {plan && (
              <details className="context-plan">
                <summary>
                  Current plan ·{" "}
                  {
                    plan.steps.filter((step) =>
                      ["completed", "done"].includes(step.status),
                    ).length
                  }{" "}
                  of {plan.steps.length} done
                </summary>
                <ol>
                  {plan.steps.map((step, index) => (
                    <li key={index}>
                      <span className={`plan-dot ${step.status}`} />
                      {step.title}
                    </li>
                  ))}
                </ol>
              </details>
            )}
          </div>
        )}
      </div>
    </aside>
  );
}

function Empty({
  icon,
  title,
  text,
}: {
  icon: "fork" | "branch" | "term";
  title: string;
  text: string;
}) {
  return (
    <div className="surface-empty">
      <Icon name={icon} />
      <b>{title}</b>
      <p>{text}</p>
    </div>
  );
}
function Diff({ text }: { text: string }) {
  return (
    <pre className="patch-diff" aria-label="Proposed diff">
      {text.split("\n").map((line, index) => (
        <span
          key={index}
          className={
            line.startsWith("+") && !line.startsWith("+++")
              ? "diff-add"
              : line.startsWith("-") && !line.startsWith("---")
                ? "diff-remove"
                : line.startsWith("@@") || line.startsWith("diff ")
                  ? "diff-section"
                  : ""
          }
        >
          {line || " "}
        </span>
      ))}
    </pre>
  );
}

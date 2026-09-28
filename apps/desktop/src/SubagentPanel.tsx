import { useWorkspaceApi } from "./Workspace";
import { useEffect, useMemo, useRef, useState } from "react";
import { type WorkspaceApi, type HistoryEvent } from "./api";
import { Icon } from "./Icon";
import type { LiveAgent } from "./live";
import { Spinner } from "./Spinner";
import { Transcript } from "./Transcript";
import {
  doingLine,
  duration,
  outcomeLabel,
  toBlocks,
  type SubagentRun,
} from "./timeline";

const LIVE_REFRESH_MS = 2000;

async function readEvents(
  api: WorkspaceApi,
  session: string,
  after: string | null = null,
) {
  const all: HistoryEvent[] = [];
  let cursor: string | null = after;
  do {
    const page = await api.events(session, cursor);
    all.push(...page.events);
    cursor = page.next_cursor;
  } while (cursor);
  return all;
}

type Props = {
  run: SubagentRun;
  live?: LiveAgent;
  onClose: () => void;
  showReasoning?: boolean;
  onAction: (
    action: "steer" | "stop" | "followup",
    text?: string,
  ) => Promise<void>;
};

export function SubagentPanel({
  run,
  live,
  onClose,
  onAction,
  showReasoning,
}: Props) {
  const api = useWorkspaceApi();
  const [events, setEvents] = useState<HistoryEvent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState("");
  const [busyAction, setBusyAction] = useState(false);
  const [actionNotice, setActionNotice] = useState<string>();
  const [fullTask, setFullTask] = useState(false);
  const body = useRef<HTMLDivElement>(null);
  const cursor = useRef<string | null>(null);
  const hasChildren = useRef(false);
  const running = live?.status === "running";

  useEffect(() => {
    setMessage("");
    setActionNotice(undefined);
    setFullTask(false);
    setEvents([]);
    setError(null);
    setLoading(true);
    cursor.current = null;
    hasChildren.current = false;
  }, [run.childId]);

  // Fetch new events while working; refresh the projection after nested agent
  // results and on completion, because those can update earlier spawn rows.
  useEffect(() => {
    let active = true;
    let busy = false;
    const load = async () => {
      if (busy) return;
      busy = true;
      try {
        let replace = !running || cursor.current === null;
        let incoming = await readEvents(
          api,
          run.childId,
          replace ? null : cursor.current,
        );
        if (
          !replace &&
          hasChildren.current &&
          incoming.some((event) => event.kind === "tool_result")
        ) {
          incoming = await readEvents(api, run.childId);
          replace = true;
        }
        if (active) {
          cursor.current = incoming.at(-1)?.id ?? cursor.current;
          hasChildren.current ||= incoming.some(
            (event) => event.kind === "subagent",
          );
          setEvents((previous) =>
            replace ? incoming : [...previous, ...incoming],
          );
          setError(null);
        }
      } catch (cause) {
        if (active) setError(String(cause));
      } finally {
        busy = false;
        if (active) setLoading(false);
      }
    };
    void load();
    const timer = running
      ? setInterval(() => void load(), LIVE_REFRESH_MS)
      : undefined;
    return () => {
      active = false;
      if (timer) clearInterval(timer);
    };
  }, [run.childId, running]);

  const blocks = useMemo(() => toBlocks(events), [events]);

  useEffect(() => {
    const box = body.current;
    if (
      box &&
      running &&
      box.scrollHeight - box.scrollTop - box.clientHeight < 120
    )
      box.scrollTop = box.scrollHeight;
  }, [blocks, running]);

  async function act(action: "steer" | "stop" | "followup") {
    setBusyAction(true);
    setActionNotice(undefined);
    try {
      await onAction(action, message.trim());
      if (action !== "stop") setMessage("");
      setActionNotice(
        action === "stop" ? "Stopping this agent…" : "Instruction sent.",
      );
    } catch (cause) {
      setActionNotice(String(cause));
    } finally {
      setBusyAction(false);
    }
  }

  const status = live?.status ?? run.status;
  const objective = run.objective || live?.objective || "";
  return (
    <section className="agent-detail" aria-label={`Sub-agent ${run.name}`}>
      <header className="panel-head">
        <div>
          <span className="panel-kicker">
            {live?.write ? "Writing agent" : "Agent"}
          </span>
          <h2>{run.name}</h2>
        </div>
        <button
          type="button"
          className="icon-btn"
          onClick={onClose}
          aria-label="Back to agents"
          title="Back to agents"
        >
          <Icon name="chev" className="icon back-icon" />
        </button>
      </header>
      <div className="panel-body" ref={body}>
        <div className="panel-brief">
          {running ? (
            <Spinner />
          ) : (
            <span className={`agent-dot ${status ?? "none"}`} />
          )}
          <span>
            {live ? doingLine(live) : outcomeLabel(status)}
            {!running && run.durationMs !== undefined
              ? ` after ${duration(run.durationMs)}`
              : ""}
          </span>
        </div>
        {objective && (
          <div className="panel-task">
            <p className={`panel-objective ${fullTask ? "" : "clamped"}`}>
              {objective}
            </p>
            {objective.length > 320 && (
              <button
                type="button"
                className="link-btn"
                onClick={() => setFullTask(!fullTask)}
              >
                {fullTask ? "Show less" : "Show the full task"}
              </button>
            )}
          </div>
        )}
        {error && <p className="panel-error">{error}</p>}
        <Transcript blocks={blocks} hideHandoff showReasoning={showReasoning} />
        {loading && <p className="quiet">Reading the sub-agent’s work…</p>}
      </div>
      <form
        className="agent-message"
        onSubmit={(event) => {
          event.preventDefault();
          void act(running ? "steer" : "followup");
        }}
      >
        {actionNotice && (
          <p className="quiet" role="status">
            {actionNotice}
          </p>
        )}
        <textarea
          rows={2}
          aria-label={`Instruction for ${run.name}`}
          placeholder={
            running
              ? `Give ${run.name} an instruction…`
              : "Give this agent more work…"
          }
          value={message}
          disabled={busyAction}
          onChange={(event) => setMessage(event.target.value)}
        />
        <div>
          <span className="quiet">
            {running
              ? "Delivered at the next step"
              : "Continues with its existing context"}
          </span>
          {running && (
            <button
              type="button"
              className="btn-line"
              disabled={busyAction}
              onClick={() => void act("stop")}
            >
              Stop agent
            </button>
          )}
          <button
            type="submit"
            className="btn-gold"
            disabled={busyAction || !message.trim()}
          >
            {busyAction ? "Sending…" : running ? "Send" : "Follow up"}
          </button>
        </div>
      </form>
    </section>
  );
}

import type { HistoryEvent } from "./api";
import type { LiveAgent, Plan, Step } from "./live";

export type SubagentRun = {
  id: string;
  name: string;
  childId: string;
  objective: string;
  status?: string;
  durationMs?: number;
};

export type Block =
  | {
      kind: "user" | "handoff" | "assistant";
      id: string;
      text: string;
      html?: string;
      ts: number;
    }
  | {
      kind: "reasoning";
      id: string;
      text: string;
      durationMs?: number;
      ts: number;
    }
  | { kind: "plan"; id: string; plan: Plan; ts: number }
  | {
      kind: "verification";
      id: string;
      ok: boolean;
      summary: string;
      output?: string;
      ts: number;
    }
  | { kind: "tools"; id: string; ts: number; steps: Step[] }
  | { kind: "subagents"; id: string; ts: number; runs: SubagentRun[] };

type Grouped = Extract<Block, { kind: "tools" | "subagents" }>;

function openGroup<K extends Grouped["kind"]>(
  blocks: Block[],
  kind: K,
  id: string,
  ts: number,
) {
  const last = blocks.at(-1);
  if (last?.kind === kind) return last as Extract<Block, { kind: K }>;
  const group = (
    kind === "tools" ? { kind, id, ts, steps: [] } : { kind, id, ts, runs: [] }
  ) as Extract<Block, { kind: K }>;
  blocks.push(group);
  return group;
}

const STATUS: Record<string, Step["status"]> = {
  ok: "ok",
  denied: "denied",
  rejected: "denied",
};

export function toBlocks(events: HistoryEvent[]): Block[] {
  const blocks: Block[] = [];
  const steps = new Map<string, Step>();
  for (const event of events) {
    switch (event.kind) {
      case "tool_call": {
        if (event.plan) {
          blocks.push({
            kind: "plan",
            id: event.id,
            plan: event.plan,
            ts: event.ts,
          });
          break;
        }
        const step: Step = {
          id: event.tool_id ?? event.id,
          tool: event.text ?? "tool",
          verb: event.verb,
          target: event.target,
          filePath: event.file_path,
          inputLabel: event.input_label,
          input: event.input,
          status: "failed",
          detail: "No result recorded",
          started: event.ts,
        };
        steps.set(step.id, step);
        openGroup(blocks, "tools", event.id, event.ts).steps.push(step);
        break;
      }
      case "tool_result": {
        const step = event.tool_id ? steps.get(event.tool_id) : undefined;
        if (step) {
          step.status = STATUS[event.status ?? ""] ?? "failed";
          step.detail = event.detail;
          step.summary = event.summary;
          step.output = event.output;
          step.ended = event.ts;
        }
        break;
      }
      case "subagent":
        openGroup(blocks, "subagents", event.id, event.ts).runs.push({
          id: event.id,
          name: event.text ?? "sub-agent",
          childId: event.child_id ?? "",
          objective: event.detail ?? "",
          status: event.status,
          durationMs: event.duration_ms,
        });
        break;
      case "reasoning":
        blocks.push({
          kind: "reasoning",
          id: event.id,
          text: event.text ?? "",
          durationMs: event.duration_ms,
          ts: event.ts,
        });
        break;
      case "verification":
        blocks.push({
          kind: "verification",
          id: event.id,
          ok: event.status === "passed",
          summary: event.text ?? "",
          output: event.output,
          ts: event.ts,
        });
        break;
      default:
        blocks.push({
          kind: event.kind,
          id: event.id,
          text: event.text ?? "",
          html: event.html,
          ts: event.ts,
        });
    }
  }
  // A spawn that worked is shown by its sub-agent row; a failed one stays visible.
  const spawned = blocks.some((block) => block.kind === "subagents");
  return blocks
    .map((block) =>
      block.kind === "tools" && spawned
        ? {
            ...block,
            steps: block.steps.filter(
              (step) => !(step.tool === "agent.spawn" && step.status === "ok"),
            ),
          }
        : block,
    )
    .filter((block) => block.kind !== "tools" || block.steps.length > 0);
}

export function toolLabel(tool: string) {
  const segment = (tool.split(".").at(-1) ?? tool).replaceAll("_", " ");
  return segment.charAt(0).toUpperCase() + segment.slice(1);
}

const RUNNING: Record<string, string> = {
  Read: "Reading",
  Ran: "Running",
  Searched: "Searching",
  "Searched the web": "Searching the web",
  Fetched: "Fetching",
  Edited: "Editing",
  Wrote: "Writing",
  "Waited for agents": "Waiting for agents",
  "Started agent": "Starting agent",
  "Started agents": "Starting agents",
};

export function stepVerb(step: Step) {
  const verb = step.verb ?? toolLabel(step.tool);
  return step.status === "running" ? (RUNNING[verb] ?? verb) : verb;
}

/** What a running agent is doing now, in the words its step rows use. */
export function doingLine(agent: LiveAgent) {
  if (agent.status !== "running") return outcomeLabel(agent.status);
  const doing = agent.doing;
  switch (doing?.state) {
    case "tool":
      return [RUNNING[doing.verb] ?? doing.verb, doing.target]
        .filter(Boolean)
        .join(" ");
    case "waiting":
      return `Waiting on you to allow: ${doing.action}`;
    case "thinking":
      return "Thinking…";
    case "idle":
      return "Idle";
    default:
      return "Starting…";
  }
}

export function tokens(count: number) {
  return count < 1000
    ? `${count} tokens`
    : `${(count / 1000).toFixed(count < 10_000 ? 1 : 0)}k tokens`;
}

const OUTCOMES: Record<string, string> = {
  completed: "Done",
  exhausted: "Ran out of budget",
  failed: "Failed",
  cancelled: "Cancelled",
};

export function outcomeLabel(status?: string) {
  return status ? (OUTCOMES[status] ?? status) : "No result recorded";
}

export function duration(ms?: number) {
  if (ms === undefined) return "";
  const seconds = Math.round(ms / 1000);
  return seconds < 60
    ? `${seconds}s`
    : `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, "0")}s`;
}

export function clock(ts: number) {
  return new Date(ts * 1000).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
  });
}

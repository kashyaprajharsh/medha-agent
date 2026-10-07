import type { LiveFrame, SessionSettings, ToolScreen } from "./api";

/** What may be answered to an approval, as the backend names each answer. */
export type ApprovalChoice =
  | "once"
  | "always"
  | "folder"
  | "session"
  | "persistent"
  | "deny";
export type Approval = {
  gateId: number;
  action: string;
  detail?: string;
  escalated: boolean;
  /** A path prompt can target either a file or a directory. */
  kind?: string;
  path?: {
    path: string;
    kind: "file" | "directory" | "unknown";
    access: "read" | "write";
  };
  /** The answers the backend offers, in the order it offers them. */
  choices: ApprovalChoice[];
  /** The folder the "folder" answer would remember. */
  folder?: string;
  pending?: boolean;
};
export type QuestionForm = {
  id: number;
  questions: {
    prompt: string;
    header: string;
    multi_select: boolean;
    options: { label: string; description: string; recommended: boolean }[];
  }[];
  pending?: boolean;
};

export type Step = {
  id: string;
  tool: string;
  verb?: string;
  target?: string;
  filePath?: string;
  status: "running" | "ok" | "failed" | "denied" | "stopped";
  errorCode?: string;
  detail?: string;
  inputLabel?: string;
  input?: string;
  summary?: string;
  output?: string;
  /** A screen the tool's server offers for this result. */
  screen?: ToolScreen;
  started?: number;
  ended?: number;
};

export type Plan = {
  explanation?: string;
  steps: { title: string; status: string }[];
};

export type Doing =
  | { state: "thinking" | "idle" | "finished" }
  | { state: "tool"; verb: string; target?: string }
  | { state: "waiting"; action: string };

export type LiveAgent = {
  name: string;
  path?: string;
  write?: boolean;
  session: string;
  objective: string;
  startedMs: number;
  status: string;
  doing?: Doing;
  toolCalls?: number;
  tokens?: number;
};

export type LiveItem =
  | { kind: "user"; id: string; text: string; ts: number }
  | { kind: "text"; id: string; text: string; html?: string }
  | { kind: "plan"; id: string; plan: Plan }
  | {
      kind: "reasoning";
      id: string;
      text: string;
      started: number;
      ended?: number;
    }
  | { kind: "tools"; id: string; steps: Step[] };

export type LiveState = {
  status: "starting" | "idle" | "running" | "exited";
  sessionId?: string;
  model?: string;
  sent: Extract<LiveItem, { kind: "user" }>[];
  items: LiveItem[];
  approvals: Approval[];
  agents: LiveAgent[];
  questions: QuestionForm[];
  settings?: SessionSettings;
  usage?: { prompt: number; total: number; cached?: number };
  verification?: { ok: boolean; summary: string };
  compaction?: {
    before: number;
    after: number;
    summary?: string;
    summarized?: boolean;
  };
  stopReason?: string;
  contextPercent?: number;
  contextPressure?: { input: number; usable?: number; quality?: string };
  activity?: { kind: "compacting" | "model"; started: number };
  notice?: string;
  error?: string;
  settled: number;
  // Messages a stopped or failed turn never read, waiting for the composer.
  returned?: string[];
  /** Calls to a server's tool that the model is still writing, for a screen to draw from. */
  writing?: WritingCall[];
};

export type WritingCall = { id: string; tool: string; text: string };
// A turn writes few such calls at once and a drawing is small; past this a
// screen simply waits for the finished call.
const MAX_WRITING = 4;
const MAX_WRITING_TEXT = 1024 * 1024;

export const startingLive = (): LiveState => ({
  status: "starting",
  sent: [],
  items: [],
  approvals: [],
  agents: [],
  questions: [],
  settled: 0,
});

const num = (value: unknown) => (typeof value === "number" ? value : undefined);

function toAgents(value: unknown): LiveAgent[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((row) => {
    if (!row || typeof row !== "object") return [];
    const agent = row as Record<string, unknown>;
    const session = str(agent.session);
    if (!session) return [];
    return [
      {
        name: str(agent.name) ?? "agent",
        path: str(agent.path),
        write: agent.write === true,
        session,
        objective: str(agent.objective) ?? "",
        startedMs: num(agent.started_ms) ?? Date.now(),
        status: str(agent.status) ?? "running",
        doing:
          agent.doing && typeof agent.doing === "object"
            ? (agent.doing as Doing)
            : undefined,
        toolCalls: num(agent.tool_calls),
        tokens: num(agent.tokens),
      },
    ];
  });
}

const str = (value: unknown) => (typeof value === "string" ? value : undefined);
const now = () => Date.now() / 1000;
let serial = 0;
const nextId = (kind: string) => `${kind}-${(serial += 1)}`;

function closeReasoning(items: LiveItem[]): LiveItem[] {
  return items.map((item) =>
    item.kind === "reasoning" && item.ended === undefined
      ? { ...item, ended: now() }
      : item,
  );
}

function closeAll(items: LiveItem[]): LiveItem[] {
  return closeReasoning(items).map((item) =>
    item.kind === "tools"
      ? {
          ...item,
          steps: item.steps.map((step) =>
            step.status === "running"
              ? {
                  ...step,
                  status: "stopped" as const,
                  ended: step.ended ?? now(),
                }
              : step,
          ),
        }
      : item,
  );
}

function withLast<K extends LiveItem["kind"]>(
  items: LiveItem[],
  kind: K,
  update: (item: Extract<LiveItem, { kind: K }>) => LiveItem,
  create: () => LiveItem,
): LiveItem[] {
  const last = items.at(-1);
  return last?.kind === kind
    ? [...items.slice(0, -1), update(last as Extract<LiveItem, { kind: K }>)]
    : [...items, create()];
}

export function recordSent(
  state: LiveState,
  message: Extract<LiveItem, { kind: "user" }>,
): LiveState {
  const steering =
    state.status === "running" ||
    (state.status === "starting" && state.sent.length > 0);
  return {
    ...state,
    sent: [...state.sent, message],
    // A follow-up stays queued until the runtime reaches its steering boundary.
    // A new turn starts with its user message immediately in the timeline.
    items: steering ? state.items : [...state.items, message],
  };
}

export function queuedMessages(state: LiveState) {
  const positioned = new Set(
    state.items.filter((item) => item.kind === "user").map((item) => item.id),
  );
  return state.sent.filter((message) => !positioned.has(message.id));
}

/** Folds one bridge frame into the live view, in the order things happened.
 * `settled` bumps when a turn ends: the cue to reload the durable transcript. */
export function reduceLive(state: LiveState, frame: LiveFrame): LiveState {
  const params = frame.params ?? {};
  switch (frame.method) {
    case "session.presentation": {
      let recovered = { ...state, items: [], approvals: [], questions: [], activity: undefined } as LiveState;
      for (const row of Array.isArray(params.frames) ? params.frames : []) {
        recovered = reduceLive(recovered, row as LiveFrame);
      }
      for (const prompt of Array.isArray(params.approvals) ? params.approvals : []) {
        recovered = reduceLive(recovered, { method: "approval", params: prompt });
      }
      for (const prompt of Array.isArray(params.questions) ? params.questions : []) {
        recovered = reduceLive(recovered, { method: "question", params: prompt });
      }
      return {
        ...recovered,
        sessionId: str(params.session),
        status: params.running === true ? "running" : "idle",
        settings: params.settings as SessionSettings | undefined,
        model: str((params.settings as SessionSettings | undefined)?.model),
        agents: toAgents(params.agents),
      };
    }
    case "session.rewound":
      return params.code_only
        ? { ...state, settled: state.settled + 1 }
        : {
            ...startingLive(),
            model: state.model,
            settings: state.settings,
            sessionId: str(params.session),
            status: "idle",
            settled: state.settled + 1,
          };
    case "ready":
      return {
        ...state,
        status: state.sent.length ? "running" : "idle",
        sessionId: str(params.session),
        model: str(params.model),
      };
    case "approval": {
      const target =
        params.path && typeof params.path === "object"
          ? (params.path as Record<string, unknown>)
          : undefined;
      const path: Approval["path"] =
        target && typeof target.path === "string" &&
        (target.access === "read" || target.access === "write")
          ? {
              path: target.path,
              kind:
                target.kind === "file" || target.kind === "directory"
                  ? target.kind
                  : "unknown",
              access: target.access,
            }
          : undefined;
      return {
        ...state,
        approvals: [
          ...state.approvals,
          {
            gateId: Number(params.gate_id),
            action: str(params.action) ?? "an action",
            detail: str(params.detail),
            escalated: params.escalated === true,
            kind: str(params.kind),
            path,
            choices: Array.isArray(params.choices)
              ? (params.choices as ApprovalChoice[])
              : ["once", "deny"],
            folder: str(params.folder),
          },
        ],
      };
    }
    case "exit":
      return {
        ...state,
        status: "exited",
        activity: undefined,
        approvals: [],
        questions: [],
        items: [...closeAll(state.items), ...queuedMessages(state)],
        error:
          state.status === "starting" || state.status === "running"
            ? (lastLine(str(params.stderr)) ??
              "Medha stopped before the turn finished.")
            : state.error,
      };
    case "agents":
      return { ...state, agents: toAgents(params.agents) };
    case "settings":
      return {
        ...state,
        settings: params as unknown as SessionSettings,
        model: str(params.model),
      };
    case "question":
      return {
        ...state,
        questions: [
          ...state.questions,
          {
            id: Number(params.question_id),
            questions: params.questions as QuestionForm["questions"],
          },
        ],
      };
    case "question.answered":
      return {
        ...state,
        questions: state.questions.filter(
          (question) => question.id !== params.question_id,
        ),
      };
    // Settled from another window, or by the chat itself.
    case "approval.resolved":
      return {
        ...state,
        approvals: state.approvals.filter(
          (approval) => approval.gateId !== Number(params.gate_id),
        ),
      };
    case "event":
      break;
    default:
      // Request errors are correlated by api.liveCall and handled at their control.
      return state;
  }
  switch (params.kind) {
    case "turn.started":
      return { ...state, status: "running", stopReason: undefined, error: undefined };
    case "message.accepted": {
      const text = str(params.content) ?? "";
      const alreadyShown = state.status !== "idle" && state.sent.some(
        (sent) => sent.text === text && state.items.some((item) => item.kind === "user" && item.id === sent.id),
      );
      return { ...state, status: "running", items: alreadyShown ? state.items : [
        ...closeReasoning(state.items), { kind: "user", id: nextId("user"), text, ts: now() },
      ] };
    }
    case "model.waiting":
      return { ...state, activity: { kind: "model", started: now() } };
    case "notice":
      return { ...state, notice: str(params.text) };
    case "model.text": {
      const delta = str(params.delta) ?? "";
      const html = str(params.html);
      const items = withLast(
        closeReasoning(state.items),
        "text",
        (item) => ({ ...item, text: item.text + delta, html }),
        () => ({ kind: "text", id: nextId("text"), text: delta, html }),
      );
      return {
        ...state,
        status: "running",
        items,
        activity: undefined,
        notice: undefined,
      };
    }
    case "model.reasoning": {
      const delta = str(params.delta) ?? "";
      const last = state.items.at(-1);
      const items: LiveItem[] =
        last?.kind === "reasoning" && last.ended === undefined
          ? [...state.items.slice(0, -1), { ...last, text: last.text + delta }]
          : [
              ...state.items,
              {
                kind: "reasoning",
                id: nextId("reasoning"),
                text: delta,
                started: now(),
              },
            ];
      return {
        ...state,
        status: "running",
        items,
        activity: undefined,
        notice: undefined,
      };
    }
    case "tool.call": {
      const id = str(params.id) ?? nextId("step");
      if (
        state.items.some(
          (item) =>
            item.kind === "tools" && item.steps.some((step) => step.id === id),
        )
      )
        return state;
      if (params.plan && typeof params.plan === "object") {
        return {
          ...state,
          status: "running",
          activity: undefined,
          items: [
            ...closeReasoning(state.items),
            { kind: "plan", id, plan: params.plan as Plan },
          ],
        };
      }
      const step: Step = {
        id,
        tool: str(params.tool) ?? "tool",
        verb: str(params.verb),
        target: str(params.target),
        filePath: str(params.file_path),
        inputLabel: str(params.input_label),
        input: str(params.input),
        status: "running",
        started: now(),
      };
      const items = withLast(
        closeReasoning(state.items),
        "tools",
        (item) => ({ ...item, steps: [...item.steps, step] }),
        () => ({ kind: "tools", id: nextId("tools"), steps: [step] }),
      );
      return { ...state, status: "running", items, activity: undefined };
    }
    case "tool.observation": {
      const id = str(params.id);
      const tool = str(params.tool);
      let matched = false;
      const items = state.items.map((item) => {
        if (item.kind !== "tools" || matched) return item;
        const steps = item.steps.map((step) => {
          if (
            matched ||
            step.status !== "running" ||
            (id ? step.id !== id : step.tool !== tool)
          )
            return step;
          matched = true;
          return {
            ...step,
            status: params.ok === true ? ("ok" as const) : ("failed" as const),
            errorCode: str(params.error_code),
            detail: str(params.detail),
            summary: str(params.summary),
            output: str(params.output),
            ended: now(),
          };
        });
        return { ...item, steps };
      });
      // A call that failed has no screen coming; one that worked keeps what was
      // written until its screen arrives, so the drawing is never taken down between.
      const writing = params.ok === true ? state.writing : state.writing?.filter((call) => call.id !== id);
      return { ...state, items, writing };
    }
    case "tool.input": {
      const [id, tool, delta] = [str(params.id), str(params.tool), str(params.delta)];
      if (!id || !tool || !delta) return state;
      const calls = state.writing ?? [];
      const call = calls.find((other) => other.id === id) ?? { id, tool, text: "" };
      if (call.text.length + delta.length > MAX_WRITING_TEXT) return state;
      const rest = calls.filter((other) => other.id !== id).slice(1 - MAX_WRITING);
      return { ...state, writing: [...rest, { ...call, text: call.text + delta }] };
    }
    case "tool.screen": {
      const screen = params.screen as ToolScreen | undefined;
      if (!screen || typeof screen.resource !== "string") return state;
      const items = state.items.map((item) =>
        item.kind === "tools"
          ? { ...item, steps: item.steps.map((step) => (step.id === params.id ? { ...step, screen } : step)) }
          : item,
      );
      return { ...state, items, writing: state.writing?.filter((call) => call.id !== params.id) };
    }
    case "context_pressure":
      return {
        ...state,
        contextPercent:
          typeof params.percent === "number" ? params.percent : undefined,
        contextPressure: {
          input: num(params.input_tokens) ?? 0,
          usable: num(params.usable_input_tokens),
          quality: str(params.quality),
        },
      };
    case "compacting":
      return {
        ...state,
        activity: params.active
          ? { kind: "compacting", started: now() }
          : undefined,
      };
    case "message.queued":
      return { ...state, notice: "Medha will read this at its next step." };
    case "message.steered": {
      const message = queuedMessages(state).find(
        (message) => message.text === params.content,
      );
      return {
        ...state,
        items: message
          ? [...closeReasoning(state.items), message]
          : [...closeReasoning(state.items), { kind: "user", id: nextId("user"), text: str(params.content) ?? "", ts: now() }],
        notice: "Your instruction reached Medha.",
      };
    }
    case "message.returned": {
      const contents = Array.isArray(params.contents)
        ? params.contents.filter((text): text is string => typeof text === "string")
        : [];
      const queued = queuedMessages(state);
      const unread = contents.flatMap((text) => {
        const index = queued.findIndex((message) => message.text === text);
        return index < 0 ? [] : queued.splice(index, 1);
      });
      if (!unread.length) return state;
      const ids = new Set(unread.map((message) => message.id));
      return {
        ...state,
        sent: state.sent.filter((message) => !ids.has(message.id)),
        returned: [
          ...(state.returned ?? []),
          ...unread.map((message) => message.text),
        ],
        notice: "Medha stopped before reading your message. It’s back in the composer.",
      };
    }
    case "model.restarted": {
      // Retry abandons only the current response segment, preserving completed tools.
      const items = [...state.items];
      while (
        items.at(-1)?.kind === "text" ||
        items.at(-1)?.kind === "reasoning"
      )
        items.pop();
      return { ...state, items, writing: undefined, notice: "Reconnecting to the model…" };
    }
    case "usage":
      return {
        ...state,
        usage: {
          prompt: num(params.prompt_tokens) ?? 0,
          total: num(params.total_tokens) ?? 0,
          cached: num(params.cached_prompt_tokens),
        },
      };
    case "verify":
      return {
        ...state,
        verification: {
          ok: params.ok === true,
          summary: str(params.summary) ?? "",
        },
      };
    case "compaction":
      return {
        ...state,
        compaction: {
          before: num(params.before) ?? 0,
          after: num(params.after) ?? 0,
          summary: str(params.summary),
          summarized:
            typeof params.summarized === "boolean"
              ? params.summarized
              : undefined,
        },
      };
    case "turn.done":
      return {
        ...settle(state, stopNotice(str(params.stopped))),
        stopReason: str(params.stopped),
      };
    case "turn.continued":
      return { ...state, notice: stopNotice(str(params.stopped)) };
    case "turn.cancelled":
      return {
        ...settle(state, "Stopped. You can continue when you’re ready."),
        stopReason: "cancelled",
      };
    case "turn.error":
      return {
        ...settle(state),
        error: str(params.message) ?? "The turn failed.",
      };
    default:
      return state;
  }
}

function settle(state: LiveState, notice?: string): LiveState {
  return {
    ...state,
    status: "idle",
    activity: undefined,
    approvals: [],
    questions: [],
    writing: undefined,
    items: [...closeAll(state.items), ...queuedMessages(state)],
    notice,
    settled: state.settled + 1,
  };
}

function stopNotice(reason?: string) {
  if (!reason) return undefined;
  if (reason === "verification_failed")
    return "Verification failed. Review the check before continuing.";
  if (reason === "blocked_by_hook") return "A project hook blocked this turn.";
  return `Stopped at the ${reason.replaceAll("_", " ")} limit. You can continue with another message.`;
}

function lastLine(text?: string) {
  const line = text?.trim().split("\n").filter(Boolean).at(-1);
  return line || undefined;
}

export function explainError(message: string) {
  if (/no model configured/i.test(message)) {
    return "No model is connected yet. Choose “No model yet” below to connect one.";
  }
  if (
    /transport error|error sending request|connection refused/i.test(message)
  ) {
    return "Medha couldn’t reach the model. Check its server or choose another model below.";
  }
  if (/401|403|unauthori[sz]ed|invalid api key/i.test(message)) {
    return "The model provider rejected the credentials. Check the saved profile’s credentials or choose another model.";
  }
  return undefined;
}

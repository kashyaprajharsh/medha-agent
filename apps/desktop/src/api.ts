import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Session = {
  id: string;
  title: string;
  parent_id: string | null;
  started_ts: number;
  last_ts: number;
  events: number;
};

export type HistoryEvent = {
  id: string;
  kind:
    | "user"
    | "handoff"
    | "assistant"
    | "reasoning"
    | "tool_call"
    | "tool_result"
    | "subagent"
    | "verification";
  verb?: string;
  target?: string;
  file_path?: string;
  input_label?: string;
  input?: string;
  summary?: string;
  output?: string;
  plan?: { explanation?: string; steps: { title: string; status: string }[] };
  ts: number;
  text?: string;
  html?: string;
  tool_id?: string;
  status?: string;
  error_code?: string;
  child_id?: string;
  detail?: string;
  duration_ms?: number;
};

export type EventPage = { events: HistoryEvent[]; next_cursor: string | null };

export type FileChange = {
  path: string;
  created: boolean;
  added: number;
  removed: number;
  edits: number;
  diff: string;
  last_ts: number;
  history: FileEdit[];
};

export type FileEdit = {
  kind: "created" | "wrote" | "edited";
  added: number;
  removed: number;
  diff: string;
  ts: number;
};
export type TerminalFrame =
  | { kind: "output"; data: number[] }
  | { kind: "exit"; code: number | null };

export type LiveFrame = {
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: Record<string, unknown>;
  error?: { message?: string };
};

export type SessionSettings = {
  profiles: {
    name: string;
    model: string;
    protocol: string;
    default: boolean;
    reasoning_support: string;
  }[];
  profile: string | null;
  model: string | null;
  mode: "plan" | "careful" | "normal" | "yolo";
  reasoning: "auto" | "on" | "off";
  effort: string;
  efforts: string[];
  reasoning_support: string;
  streaming: boolean;
  context_limit: number | null;
};
export type SessionChange =
  | { profile: string }
  | { mode: string }
  | { reasoning: string }
  | { effort: string }
  | { streaming: boolean };
export type Patch = {
  id: string;
  agent: string;
  session: string;
  files: string[];
  diff: string;
  verification?: { passed: boolean; command: string; output: string };
};
export type Attachment = {
  id: string;
  name: string;
  mime: string;
  data: string;
  preview: string;
  note?: string;
};
export type LiveMethod =
  | "message.send"
  | "approval.respond"
  | "cancel"
  | "session.settings"
  | "session.configure"
  | "agent.control"
  | "patch.list"
  | "patch.apply"
  | "question.respond"
  | "extensions.reload"
  | "extensions.catalog"
  | "mcp.connect"
  | "mcp.disconnect"
  | "mcp.signin"
  | "tasks.list"
  | "memory.list"
  | "memory.pin"
  | "memory.forget"
  | "memory.provenance"
  | "session.rewind"
  | "session.rewind.points";

export type FileEntry = {
  name: string;
  path: string;
  directory: boolean;
  size: number;
};
export type FilePreview = {
  kind: string;
  extension: string;
  size: number;
  text?: string;
  html?: string;
  bytes?: number[];
};
export type GitFile = {
  path: string;
  index: string;
  working: string;
  untracked: boolean;
  original?: string;
};
export type Workspace = {
  id: string;
  name: string;
  path: string;
  personal: boolean;
};
export const desktop = {
  admitImage: (bytes: Uint8Array) =>
    invoke<{ mime: string; data: string; note?: string }>("image_admit", bytes),
  workspaces: () =>
    invoke<{ initial: string; workspaces: Workspace[] }>("workspace_list"),
  chooseWorkspace: () => invoke<Workspace | null>("workspace_choose"),
};
export type Scope = { chatKey: string | null; sessionId: string | null };
export function createApi(workspaceId: string, getScope: () => Scope) {
  const prefix = `${workspaceId}-`;
  const keyFor = (key: string) => `${prefix}${key}`;
  const scope = () => {
    const value = getScope();
    return {
      workspaceId,
      chatKey: value.chatKey ? keyFor(value.chatKey) : null,
      sessionId: value.sessionId,
    };
  };
  const api = {
    terminalOpen: (
      key: string,
      cols: number,
      rows: number,
      onFrame: (frame: TerminalFrame) => void,
    ) => {
      const output = new Channel<TerminalFrame>();
      output.onmessage = onFrame;
      return invoke<{ shell: string; workspace: string }>("terminal_open", {
        ...scope(),
        key: keyFor(key),
        cols,
        rows,
        output,
      });
    },
    terminalWrite: (key: string, data: string) =>
      invoke<void>("terminal_write", { key: keyFor(key), data }),
    terminalResize: (key: string, cols: number, rows: number) =>
      invoke<void>("terminal_resize", { key: keyFor(key), cols, rows }),
    terminalClose: (key: string) =>
      invoke<void>("terminal_close", { key: keyFor(key) }),
    openLink: (url: string) => invoke<void>("open_link", { url }),
    settings: (
      method = "settings.list",
      params: Record<string, unknown> = {},
    ) =>
      invoke<Record<string, unknown>>("settings_request", {
        ...scope(),
        method,
        params,
      }),
    files: (directory = "") =>
      invoke<FileEntry[]>("files_list", { ...scope(), directory }),
    preview: (path: string) =>
      invoke<FilePreview>("file_preview", { ...scope(), path }),
    openFile: (path: string) => invoke<void>("file_open", { ...scope(), path }),
    gitStatus: () =>
      invoke<{ repository: boolean; files: GitFile[] }>("git_status", scope()),
    gitDiff: (path: string, staged: boolean) =>
      invoke<{ text: string }>("git_diff", { ...scope(), path, staged }),
    extensions: (
      method = "extensions.list",
      params: Record<string, unknown> = {},
    ) =>
      invoke<Record<string, unknown>>("extension_request", {
        ...scope(),
        method,
        params,
      }),
    workspace: () => invoke<string>("workspace_info", scope()),
    revealWorkspace: () => invoke<void>("workspace_reveal", scope()),
    branch: () => invoke<string | null>("workspace_branch", { workspaceId }),
    sessions: () => invoke<Session[]>("list_sessions", { workspaceId }),
    usage: (days: number) =>
      invoke<Record<string, unknown>>("usage_summary", { workspaceId, days }),
    defaults: () =>
      invoke<SessionSettings>("session_defaults", { workspaceId }),
    events: (sessionId: string, cursor: string | null) =>
      invoke<EventPage>("session_events", { workspaceId, sessionId, cursor }),
    changes: (sessionId: string) =>
      invoke<{ files: FileChange[] }>("session_changes", {
        workspaceId,
        sessionId,
      }),
    liveOpen: (key: string, sessionId: string | null) =>
      invoke<void>("live_open", { workspaceId, key: keyFor(key), sessionId }),
    liveRequest: (
      key: string,
      method: LiveMethod,
      params: Record<string, unknown> = {},
    ) =>
      invoke<number>("live_request", {
        workspaceId,
        key: keyFor(key),
        method,
        params,
      }),
    liveCall: async (
      key: string,
      method: LiveMethod,
      params: Record<string, unknown> = {},
    ): Promise<Record<string, unknown>> => {
      let off: UnlistenFn | undefined;
      let requestId: number | undefined;
      const early: LiveFrame[] = [];
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        return await new Promise<Record<string, unknown>>((resolve, reject) => {
          const finish = (frame: LiveFrame) => {
            if (frame.id !== requestId) return;
            if (frame.error)
              reject(
                new Error(
                  frame.error.message ??
                    "Medha could not complete this action.",
                ),
              );
            else resolve(frame.result ?? {});
          };
          void api
            .onLive((frameKey, frame) => {
              if (frameKey !== key) return;
              if (frame.method === "exit") {
                reject(
                  new Error(
                    "Medha disconnected before confirming this action.",
                  ),
                );
                return;
              }
              if (frame.id === undefined || (!frame.result && !frame.error))
                return;
              if (requestId === undefined) early.push(frame);
              else finish(frame);
            })
            .then(async (unlisten) => {
              off = unlisten;
              timer = setTimeout(
                () =>
                  reject(
                    new Error(
                      "Medha has not confirmed this action. Check its status before trying again.",
                    ),
                  ),
                120_000,
              );
              requestId = await api.liveRequest(key, method, params);
              early.forEach(finish);
            })
            .catch(reject);
        });
      } finally {
        off?.();
        if (timer) clearTimeout(timer);
      }
    },
    onLive: (
      handler: (key: string, frame: LiveFrame) => void,
    ): Promise<UnlistenFn> =>
      listen<{ key: string; frame: LiveFrame }>("medha-live", (event) => {
        if (event.payload.key.startsWith(prefix))
          handler(event.payload.key.slice(prefix.length), event.payload.frame);
      }),
  };

  return api;
}
export type WorkspaceApi = ReturnType<typeof createApi>;

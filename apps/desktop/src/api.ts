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
  /** Set on an answer written by a model that stood in for the chat's own. */
  stand_in?: { model: string; for: string };
  images?: MessageImage[];
  /** The output a person's message was about, as the label shown beside it. */
  about?: string;
  html?: string;
  tool_id?: string;
  status?: string;
  error_code?: string;
  child_id?: string;
  detail?: string;
  duration_ms?: number;
  screen?: ToolScreen;
  /** Structured values remain stored but were omitted to keep this page bounded. */
  omitted?: ("plan" | "screen" | "images")[];
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
export type ImageAttachment = {
  kind?: "image";
  id: string;
  name: string;
  mime: string;
  data: string;
  preview: string;
  note?: string;
};
export type LocalAttachment = { kind: "file" | "folder"; id: string; name: string; path: string };
export type Attachment = ImageAttachment | LocalAttachment;
/** Saved rows keep content hashes; only optimistic messages have inline previews. */
export type MessageImage = { hash?: string; name: string; mime: string; preview?: string };
export type LiveMethod =
  | "message.send"
  | "approval.respond"
  | "cancel"
  | "session.settings"
  | "session.presentation"
  | "session.configure"
  | "agent.control"
  | "patch.list"
  | "patch.apply"
  | "question.respond"
  | "extensions.reload"
  | "extensions.catalog"
  | "mcp.screens"
  | "mcp.screen"
  | "mcp.screen.call"
  | "mcp.connect"
  | "connectors.connect"
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
/** The web origins a server declared for its screen; the frame may reach these and no others. */
export type ScreenOrigins = { connect: string[]; resources: string[]; frames: string[] };
/** A screen a tool's server offers for a result: where its page is, and what it draws. */
export type ToolScreen = {
  identity?: string;
  source_event?: string;
  server: string;
  tool: string;
  resource: string;
  /** The call's arguments as text, while the model is still writing them. */
  partial?: string;
  /** Arguments the tool takes as text; a list or object written there is shown as JSON text, as the finished call will run. */
  text_fields?: string[];
  input?: unknown;
  result?: unknown;
};
/** A page's local file. With a hash it was kept; without one it is read from the page's folder as it is now. */
export type ArtifactAsset = { path: string; hash?: string; size: number; mime: string };
export type ArtifactSnapshot = { path: string; kind: string; hash: string; size: number; assets: ArtifactAsset[]; missing: string[] };
export type ArtifactRevision = { artifact_id: string; revision_id: string; predecessor: string | null; source_event: string; source_index: number; tool_call?: string; ts: number; kind: string; name: string; inline?: boolean; source_digest?: string; source?: string; snapshot?: ArtifactSnapshot; /** A file shown where it lies, as it is now. */ path?: string; screen?: ToolScreen };
export type ArtifactPage = { revisions: ArtifactRevision[]; next_cursor: number | null; omitted: number };
export type FilePreview = {
  kind: string;
  extension: string;
  size: number;
  text?: string;
  html?: string;
  bytes?: number[] | Uint8Array;
  data?: string;
  bundle?: string;
  missing?: string[];
  snapshot?: boolean;
};

const decodedPreview = (file: FilePreview): FilePreview => {
  if (file.data === undefined) return file;
  const text = atob(file.data);
  const bytes = Uint8Array.from(text, (char) => char.charCodeAt(0));
  const { data: _data, ...rest } = file;
  return { ...rest, bytes };
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
export type ExtensionSourceKind = "skill_folder" | "skill_file" | "plugin_folder";
export const desktop = {
  admitImage: (bytes: Uint8Array) =>
    invoke<{ mime: string; data: string; note?: string }>("image_admit", bytes),
  workspaces: () =>
    invoke<{ initial: string; workspaces: Workspace[] }>("workspace_list"),
  chooseWorkspace: () => invoke<Workspace | null>("workspace_choose"),
  chooseExtensionSource: (kind: ExtensionSourceKind) =>
    invoke<string | null>("extension_source_choose", { kind }),
  chooseAttachments: async (kind: "files" | "folder"): Promise<Attachment[] | null> => {
    type Picked = Omit<ImageAttachment, "id" | "preview"> & { kind: "image" }
      | Omit<LocalAttachment, "id">;
    const selected = await invoke<Picked[] | null>("attachment_choose", { kind });
    return selected?.map((item) => "data" in item
      ? { ...item, id: crypto.randomUUID(), preview: `data:${item.mime};base64,${item.data}` }
      : { ...item, id: crypto.randomUUID() }) ?? null;
  },
  updateCheck: () => invoke<{ version: string } | null>("update_check"),
  updateApply: () => invoke<void>("update_apply"),
};
export type Scope = { chatKey: string | null; sessionId: string | null };
export function createApi(workspaceId: string, getScope: () => Scope) {
  const prefix = `${workspaceId}-`;
  // Native viewers outlive the renderer. A resumed conversation may still
  // belong to its original draft key, so every command uses the resolved key.
  const routes = new Map<string, string>();
  const keyFor = (key: string) => routes.get(key) ?? `${prefix}${key}`;
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
      invoke<FilePreview>("file_preview", { ...scope(), path }).then(decodedPreview),
    artifacts: (sessionId: string, cursor = 0) => invoke<ArtifactPage>("artifact_list", { workspaceId, sessionId, cursor }),
    artifactPreview: (sessionId: string, revisionId: string) => invoke<FilePreview>("artifact_preview", { workspaceId, sessionId, revisionId }).then(decodedPreview),
    artifactRelease: (bundle: string) => invoke<void>("artifact_release", { bundle }),
    openFile: (path: string) => invoke<string | null>("file_open", { ...scope(), path }),
    revealFile: (path: string) => invoke<void>("file_reveal", { ...scope(), path }),
    mediaLink: (path: string) =>
      invoke<{ url: string; size: number }>("media_link", { ...scope(), path }),
    saveOutput: (name: string, content: { text: string } | { bytes: Uint8Array } | { path: string } | { revisionId: string; sessionId: string }) =>
      invoke<boolean>("output_save", {
        ...scope(),
        sessionId: "revisionId" in content ? content.sessionId : scope().sessionId,
        request: { name, content: "revisionId" in content ? { kind: "revision", revision_id: content.revisionId }
          : "path" in content ? { kind: "file", path: content.path }
          : { kind: "bytes", data: Array.from("bytes" in content ? content.bytes : new TextEncoder().encode(content.text)) } },
      }),
    screenPut: (html: string, run: boolean, online: boolean, app?: ScreenOrigins, bundle?: string) =>
      invoke<string>("screen_put", { html, run, online, app, bundle }),
    screenDrop: (url: string) => invoke<void>("screen_drop", { url }),
    screenKeep: (server: string, uri: string, page: unknown) =>
      invoke<void>("screen_keep", { server, uri, page }),
    screenKept: (server: string, uri: string) =>
      invoke<Record<string, unknown> | null>("screen_kept", { server, uri }),
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
    historyImage: (hash: string, thumbnail = true) =>
      invoke<Omit<ImageAttachment, "preview">>("history_image", { ...scope(), hash, thumbnail }),
    changes: (sessionId: string) =>
      invoke<{ files: FileChange[] }>("session_changes", {
        workspaceId,
        sessionId,
      }),
    liveFocus: (key: string | null) =>
      invoke<void>("live_focus", { key: key ? keyFor(key) : null }),
    liveOpen: async (key: string, sessionId: string | null) => {
      const opened = await invoke<{ key: string; reused: boolean }>("live_open", {
        workspaceId, key: keyFor(key), sessionId,
      });
      if (!opened.key.startsWith(prefix)) throw new Error("Medha returned a chat from another workspace.");
      routes.set(key, opened.key);
      if (getScope().chatKey === key) void api.liveFocus(key).catch(() => {});
      return opened.reused;
    },
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
        if (!event.payload.key.startsWith(prefix)) return;
        const keys = new Set([event.payload.key.slice(prefix.length)]);
        for (const [key, native] of routes) if (native === event.payload.key) keys.add(key);
        for (const key of keys) handler(key, event.payload.frame);
      }),
  };

  return api;
}
export type WorkspaceApi = ReturnType<typeof createApi>;

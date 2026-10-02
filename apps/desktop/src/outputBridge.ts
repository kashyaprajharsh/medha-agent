// Speaks to a server's screen in its frame: the MCP Apps protocol, JSON-RPC over
// messages. The screen asks; the host decides. Nothing a screen sends is trusted
// beyond its shape, and it hears nothing until it has said it is ready.

export const PROTOCOL = "2026-01-26";
const MAX_TEXT = 4000;

type Id = string | number;
type Message = { jsonrpc?: string; id?: Id; method?: string; params?: unknown; result?: unknown };

/**
 * Reads arguments the model has only half written: whatever is open is closed,
 * and a trailing piece that cannot yet mean anything is left off. `undefined`
 * when there is nothing to read yet.
 */
export function halfWritten(text: string): unknown {
  const closers: string[] = [];
  let inString = false;
  let escaped = false;
  // The last place the text could be cut and still be closed into something whole.
  let whole = 0;
  let wholeClosers = "";
  for (let at = 0; at < text.length; at++) {
    const char = text[at];
    if (inString) {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') inString = false;
      // Inside a string every character but a dangling escape can be the last one.
      if (inString && !escaped && !/[\ud800-\udbff]/.test(char)) {
        whole = at + 1;
        wholeClosers = `"${closers.join("")}`;
      }
      continue;
    }
    if (char === '"') inString = true;
    else if (char === "{") closers.unshift("}");
    else if (char === "[") closers.unshift("]");
    else if (char === "}" || char === "]") closers.shift();
    else continue;
    if (!inString) {
      whole = at + 1;
      wholeClosers = closers.join("");
    }
  }
  for (let cut = whole; cut > 0; cut = text.lastIndexOf(",", cut - 1)) {
    try {
      return JSON.parse(text.slice(0, cut) + (cut === whole ? wholeClosers : closing(text.slice(0, cut))));
    } catch {
      /* a key with no value yet, or half a number: try one item earlier */
    }
  }
  return undefined;
}

/** What it takes to close everything still open in a prefix that ends between items. */
function closing(text: string): string {
  const closers: string[] = [];
  let inString = false;
  let escaped = false;
  for (const char of text) {
    if (inString) {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') inString = false;
    } else if (char === '"') inString = true;
    else if (char === "{") closers.unshift("}");
    else if (char === "[") closers.unshift("]");
    else if (char === "}" || char === "]") closers.shift();
  }
  return (inString ? '"' : "") + closers.join("");
}

/** What a screen is given to draw, as far as it is known. */
export type Fed = { partial?: unknown; input?: unknown; result?: unknown };

export type Host = {
  /** What the screen is told about where it is shown. */
  context: () => Record<string, unknown>;
  /** Each resolves once the person has agreed and the thing was done, and rejects otherwise. */
  callTool: (name: string, args: unknown) => Promise<unknown>;
  openLink: (url: string) => Promise<void>;
  say: (text: string) => Promise<void>;
  readPage: (uri: string) => Promise<unknown>;
  /** The page in the frame has started and is ready to be told things. */
  started?: () => void;
  resized?: (width: number, height: number) => void;
};

const record = (value: unknown): Record<string, unknown> =>
  value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
const reason = (cause: unknown) => (cause instanceof Error ? cause.message : String(cause)).slice(0, MAX_TEXT);

// A page says it is ready a moment before it starts listening: many attach what
// hears their input only once they have drawn themselves. What is sent in that
// moment is lost and never asked for again, so the first word waits this long.
const LISTENING = 250;

/** A bridge to one screen. `post` sends to its frame; `receive` takes what it sent. */
export function bridge(host: Host, post: (message: object) => void, grace = LISTENING) {
  let ready = false;
  let listening = false;
  let waiting: ReturnType<typeof setTimeout> | undefined;
  // What there is to draw, and how much of it the page in the frame has been told.
  let fed: Fed = {};
  let told = { input: false, result: false };
  const notify = (method: string, params: unknown) => post({ jsonrpc: "2.0", method, params });
  const known = (value: unknown) => value !== undefined && value !== null;
  // In the protocol's order: pieces of the input while it is written, the whole
  // input once, then the result once.
  const tell = () => {
    if (!listening) return;
    // A result always follows a whole input, even where only pieces of one were seen.
    if (!told.input && (known(fed.input) || known(fed.result))) {
      told.input = true;
      notify("ui/notifications/tool-input", { arguments: record(fed.input ?? fed.partial) });
    } else if (!told.input && known(fed.partial)) {
      notify("ui/notifications/tool-input-partial", { arguments: record(fed.partial) });
    }
    if (told.input && !told.result && known(fed.result)) {
      told.result = true;
      notify("ui/notifications/tool-result", fed.result);
    }
  };
  const answer = (id: Id, work: () => Promise<unknown>) =>
    void work().then(
      (result) => post({ jsonrpc: "2.0", id, result: result ?? {} }),
      (cause) => post({ jsonrpc: "2.0", id, error: { code: -32000, message: reason(cause) } }),
    );

  const asked: Record<string, (params: Record<string, unknown>) => Promise<unknown>> = {
    "ui/initialize": async () => {
      // A frame that was unloaded and came back holds a new page that knows nothing.
      ready = false;
      listening = false;
      clearTimeout(waiting);
      told = { input: false, result: false };
      return {
        protocolVersion: PROTOCOL,
        hostInfo: { name: "Medha", version: "1" },
        hostCapabilities: { openLinks: {}, serverTools: {}, serverResources: {}, logging: {} },
        hostContext: host.context(),
      };
    },
    ping: async () => ({}),
    "tools/call": async (params) => {
      if (typeof params.name !== "string") throw new Error("A tool call needs a name.");
      return host.callTool(params.name, record(params.arguments));
    },
    "resources/read": async (params) => {
      if (typeof params.uri !== "string") throw new Error("A read needs a uri.");
      return host.readPage(params.uri);
    },
    "ui/open-link": async (params) => {
      if (typeof params.url !== "string" || !/^https?:\/\//i.test(params.url))
        throw new Error("Only a web link can be opened.");
      await host.openLink(params.url);
    },
    "ui/message": async (params) => {
      const text = record(params.content).text;
      if (typeof text !== "string" || !text.trim()) throw new Error("A message needs text.");
      await host.say(text.slice(0, MAX_TEXT));
    },
    "ui/request-display-mode": async () => ({ mode: record(host.context()).displayMode ?? "inline" }),
  };

  return {
    /** Handles one message from the frame. Anything not understood is refused, not ignored. */
    receive(data: unknown) {
      const message = record(data) as Message;
      if (message.jsonrpc !== "2.0" || typeof message.method !== "string") return;
      const params = record(message.params);
      if (message.id === undefined) {
        if (message.method === "ui/notifications/initialized" && !ready) {
          ready = true;
          host.started?.();
          waiting = setTimeout(() => {
            listening = true;
            tell();
          }, grace);
        } else if (message.method === "ui/notifications/size-changed") {
          const [width, height] = [Number(params.width), Number(params.height)];
          if (Number.isFinite(width) && Number.isFinite(height)) host.resized?.(width, height);
        }
        return;
      }
      const handler = asked[message.method];
      const id = message.id;
      if (!handler) post({ jsonrpc: "2.0", id, error: { code: -32601, message: "Medha does not support this." } });
      else answer(id, () => handler(params));
    },
    /** Gives the screen what there now is to draw; it is told whatever is new to it. */
    feed(next: Fed) {
      fed = next;
      tell();
    },
    /** Tells a screen that is ready about a change around it, such as the theme. */
    changed(context: Record<string, unknown>) {
      if (listening) notify("ui/notifications/host-context-changed", context);
    },
    /** Asks the screen to finish up before its frame goes away. */
    close() {
      clearTimeout(waiting);
      listening = false;
      if (ready) post({ jsonrpc: "2.0", id: "medha-teardown", method: "ui/resource-teardown", params: { reason: "closed" } });
    },
  };
}

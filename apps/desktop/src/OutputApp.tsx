import { useCallback, useEffect, useRef, useState } from "react";
import type { ScreenOrigins, ToolScreen } from "./api";
import { Icon } from "./Icon";
import { useWorkspace } from "./Workspace";
import { labelOf } from "./outputBlocks";
import { bridge, halfWritten, type Fed } from "./outputBridge";
import { useOutputs, type RenderProps } from "./outputContext";
import { useNear } from "./outputFiles";
import { screened } from "./outputFrame";

type Page = ScreenOrigins & { html: string; clipboard?: boolean };
type Asking = {
  title: string;
  detail: string;
  allow: string;
  /** Remembers the yes, where that is offered. */
  always?: () => void;
  settle: (yes: boolean) => void;
};

// Screens whose page has been put on the shelf this sitting; each is written there once.
const KEPT = new Set<string>();
// How long a started screen is given to draw, and how long one may take to start at all.
const SETTLE = 1500;
const STUCK = 15_000;
// How often a screen is given the call the model is still writing.
const BEAT = 150;
// Tools the person said a server's screens may always run, per chat, for this sitting only.
const ALWAYS = new Map<string, Set<string>>();

const hostsOf = (page: ScreenOrigins) => [
  ...new Set(
    [...page.connect, ...page.resources, ...page.frames].map((origin) =>
      origin.replace(/^\w+:\/\/(\*\.)?/, "").replace(/:\d+$/, ""),
    ),
  ),
];

const AGREED = "medha-screen-agreed";
const key = (server: string) => `medha-screen:${server}`;
// Kept here too, so a yes holds for this sitting where storage is not available.
const yeses = new Map<string, string>();
function remembered(server: string): string {
  try {
    return yeses.get(server) ?? localStorage.getItem(key(server)) ?? "";
  } catch {
    return yeses.get(server) ?? "";
  }
}
function remember(server: string, hosts: string) {
  yeses.set(server, hosts);
  try {
    localStorage.setItem(key(server), hosts);
  } catch {
    /* private storage */
  }
}

type Offered = Pick<ToolScreen, "server" | "tool" | "resource"> & { name: string };
// The provider may spell a tool's name with other separators than the server did.
const plain = (name: string) => name.replace(/[^a-zA-Z0-9]/g, "_");
// Which tools come with a screen, asked once per chat and again only for a tool not yet known.
const OFFERS = new Map<string, { tools: Offered[]; asked: Set<string> }>();

/**
 * Finds the screen a tool comes with, so it can be opened while the model is
 * still writing the call. Asks the chat only when such a call is under way.
 */
export function useOffered(writing: { tool: string }[] | undefined): (tool: string) => Offered | undefined {
  const { api, scope } = useWorkspace();
  const [, learned] = useState(0);
  const chat = scope.current.chatKey ?? "";
  const names = (writing ?? []).map((call) => plain(call.tool)).join(" ");
  useEffect(() => {
    if (!chat || !names) return;
    const known = OFFERS.get(chat) ?? { tools: [], asked: new Set<string>() };
    OFFERS.set(chat, known);
    const unknown = names.split(" ").filter((name) => !known.asked.has(name));
    if (!unknown.length) return;
    unknown.forEach((name) => known.asked.add(name));
    void api
      .liveCall(chat, "mcp.screens")
      .then((answer) => {
        known.tools = (answer.tools as Offered[] | undefined) ?? [];
        learned((count) => count + 1);
      })
      .catch(() => unknown.forEach((name) => known.asked.delete(name)));
  }, [api, chat, names]);
  return (tool) => OFFERS.get(chat)?.tools.find((offer) => plain(offer.name) === plain(tool));
}

// A chat's screens are mostly the same page, shown in several plates and again
// beside the chat. It is asked for once and shared for a while, a few at a time.
const PAGES = new Map<string, { at: number; page: Promise<Page> }>();
const MAX_PAGES = 4;
const PAGE_FOR = 10 * 60_000;

function pageOf(api: ReturnType<typeof useWorkspace>["api"], chat: string | null | undefined, server: string, resource: string) {
  const key = `${chat}\0${server}\0${resource}`;
  const held = PAGES.get(key);
  if (held && performance.now() - held.at < PAGE_FOR) return held.page;
  const asked = chat
    ? api.liveCall(chat, "mcp.screen", { server, uri: resource })
    : Promise.reject(new Error("This chat is not open."));
  const page = asked
    .then((read) => {
      // Kept once a sitting, so a chat reopened without its server still shows the screen.
      if (!KEPT.has(`${server}\0${resource}`)) {
        KEPT.add(`${server}\0${resource}`);
        void api.screenKeep(server, resource, read).catch(() => {});
      }
      return read;
    })
    // The server cannot be asked: the page it sent before will do for looking.
    .catch(async () => (await api.screenKept(server, resource)) ?? Promise.reject())
    .then((read) => read as unknown as Page);
  PAGES.delete(key);
  PAGES.set(key, { at: performance.now(), page });
  if (PAGES.size > MAX_PAGES) PAGES.delete(PAGES.keys().next().value!);
  // A page that could not be had is asked for again by whoever wants it next.
  page.catch(() => PAGES.get(key)?.page === page && PAGES.delete(key));
  return page;
}

/** The page of a server's screen, fetched through the chat that is connected to that server. */
function usePage(server: string, resource: string, onProblem: RenderProps["onProblem"]) {
  const { api, scope } = useWorkspace();
  const [page, setPage] = useState<Page>();
  useEffect(() => {
    let current = true;
    void pageOf(api, scope.current.chatKey, server, resource)
      .then((read) => current && setPage(read))
      .catch(
        () => current && onProblem?.(`Send a message in this chat to connect ${server}, then its screen can be shown.`),
      );
    return () => {
      current = false;
    };
  }, [api, scope, server, resource, onProblem]);
  return page;
}

/**
 * A screen from a connected server, in a frame that reaches only what that server
 * declared. Whatever the screen asks of Medha waits for the person's yes.
 */
export function AppScreen({ output, size, onProblem, onDetail }: RenderProps) {
  const screen = output.screen!;
  const { api, scope, theme } = useWorkspace();
  const { ask } = useOutputs();
  const page = usePage(screen.server, screen.resource, onProblem);
  const box = useRef<HTMLDivElement>(null);
  const frame = useRef<HTMLIFrameElement>(null);
  const near = useNear(box, false);
  const [url, setUrl] = useState<string>();
  const [asks, setAsks] = useState<Asking[]>([]);
  // Whether the page in the frame has drawn yet; until it has, the frame is covered, not left blank.
  const [stage, setStage] = useState<"opening" | "shown" | "stuck">("opening");
  const [attempt, setAttempt] = useState(0);
  const full = size === "full";
  const who = labelOf(output);

  useEffect(() => {
    if (!page) return;
    const hosts = hostsOf(page);
    onDetail?.({
      foot: hosts.length
        ? `Talks to ${hosts.slice(0, 3).join(", ")}${hosts.length > 3 ? " and more" : ""}. It can't see your files or your other chats.`
        : "Runs here, offline. It can't see your files or your other chats.",
    });
  }, [page, onDetail]);

  // A screen that reaches the web waits for one yes per server, remembered with
  // exactly what was agreed to: a server that later asks for more asks again.
  const wanted = page ? hostsOf(page).sort().join(", ") : "";
  const [agreed, setAgreed] = useState(() => remembered(screen.server));
  const held = Boolean(wanted) && agreed !== wanted;
  const agree = () => {
    remember(screen.server, wanted);
    window.dispatchEvent(new Event(AGREED));
  };
  // The same screen is shown in the chat and beside it; a yes given in one is a yes in both.
  useEffect(() => {
    const heard = () => setAgreed(remembered(screen.server));
    window.addEventListener(AGREED, heard);
    return () => window.removeEventListener(AGREED, heard);
  }, [screen.server]);

  useEffect(() => {
    if (!page || !near || held) return;
    let current = true;
    let made: string | undefined;
    const origins = { connect: page.connect, resources: page.resources, frames: page.frames };
    void api
      .screenPut(screened(page.html), true, false, origins)
      .then((link) => {
        if (!current) return void api.screenDrop(link);
        made = link;
        setUrl(link);
      })
      .catch((cause) => current && onProblem?.(String(cause)));
    return () => {
      current = false;
      if (made) void api.screenDrop(made);
    };
  }, [api, page, near, held, onProblem]);

  // The person decides each thing a screen asks for; a screen shown small in the chat may not ask.
  // A screen may ask for several things at once; each waits its turn and gets its own answer.
  const confirm = useCallback(
    (title: string, detail: string, allow: string, always?: () => void) =>
      new Promise<void>((resolve, reject) => {
        if (!full) return reject(new Error("Open this screen in Preview to use it."));
        const mine: Asking = {
          title,
          detail,
          allow,
          always,
          settle: (yes) => {
            setAsks((list) => list.filter((other) => other !== mine));
            if (yes) resolve();
            else reject(new Error("The person did not allow this."));
          },
        };
        setAsks((list) => [...list, mine]);
      }),
    [full],
  );
  const asking = asks[0];

  const context = useRef({ theme, full });
  context.current = { theme, full };
  const link = useRef<ReturnType<typeof bridge>>(undefined);
  const drawn = useRef<Fed>({ input: screen.input, result: screen.result });
  const fedAt = useRef(0);
  const { server } = screen;
  useEffect(() => {
    if (!url || !near) return;
    let settling: ReturnType<typeof setTimeout> | undefined;
    const chat = scope.current.chatKey;
    const call = (method: "mcp.screen" | "mcp.screen.call", params: Record<string, unknown>) =>
      chat ? api.liveCall(chat, method, params) : Promise.reject(new Error("This chat is not open."));
    const made = bridge(
      {
        context: () => ({
          theme: context.current.theme === "light" ? "light" : "dark",
          displayMode: context.current.full ? "fullscreen" : "inline",
          availableDisplayModes: ["inline", "fullscreen"],
          platform: "desktop",
          locale: navigator.language,
        }),
        callTool: async (name, args) => {
          const room = `${chat}\0${server}`;
          if (!ALWAYS.get(room)?.has(name))
            await confirm(
              `${who} wants to run ${name}`,
              `It will run ${name} on the ${who} server.`,
              "Allow once",
              () => ALWAYS.set(room, (ALWAYS.get(room) ?? new Set()).add(name)),
            );
          return call("mcp.screen.call", { server, tool: name, args });
        },
        readPage: async (uri) => {
          const read = await call("mcp.screen", { server, uri });
          return { contents: [{ uri, mimeType: "text/html;profile=mcp-app", text: read.html }] };
        },
        openLink: async (target) => {
          await confirm(`${who} wants to open a link`, target, "Open link");
          await api.openLink(target);
        },
        say: async (text) => {
          await confirm(`${who} wants to add to your message`, text, "Add to message");
          ask?.(text);
        },
        // Started is not yet drawn: most screens fetch what they draw with first.
        // Reporting a size is the surest sign of a drawing; failing that, a short wait.
        started: () => {
          settling = setTimeout(() => setStage("shown"), SETTLE);
        },
        resized: () => setStage("shown"),
      },
      (message) => frame.current?.contentWindow?.postMessage(message, "*"),
    );
    link.current = made;
    made.feed(drawn.current);
    // A frame that has just been loaded, or loaded again, shows nothing yet.
    setStage("opening");
    const giveUp = setTimeout(() => setStage((now) => (now === "opening" ? "stuck" : now)), STUCK);
    // Only this frame's own window is heard; its origin is opaque, so that is the one check there is.
    const hear = (event: MessageEvent) => {
      if (event.source && event.source === frame.current?.contentWindow) made.receive(event.data);
    };
    window.addEventListener("message", hear);
    return () => {
      made.close();
      clearTimeout(settling);
      clearTimeout(giveUp);
      window.removeEventListener("message", hear);
      link.current = undefined;
    };
  }, [api, scope, url, near, attempt, server, who, confirm, ask]);
  useEffect(() => {
    link.current?.changed({ theme: theme === "light" ? "light" : "dark" });
  }, [theme]);

  // What there is to draw reaches the screen as it grows: pieces of the call
  // while the model writes it, read on a beat, then the whole call, then its result.
  const { partial, input, result } = screen;
  useEffect(() => {
    const feed = () => {
      fedAt.current = performance.now();
      drawn.current = { partial: partial === undefined ? undefined : halfWritten(partial), input, result };
      link.current?.feed(drawn.current);
    };
    if (input !== undefined || partial === undefined) return feed();
    // The wait is what is left of the beat, so pieces arriving faster than it never put it off.
    const beat = setTimeout(feed, Math.max(0, BEAT - (performance.now() - fedAt.current)));
    return () => clearTimeout(beat);
  }, [partial, input, result]);

  return (
    <div className={`out-app ${full ? "live" : ""}`} ref={box}>
      {held && (
        <div className="out-problem">
          <Icon name="globe" />
          <b>{who} shows its own screen here</b>
          <span>It talks to {wanted}. It can't see your files or your other chats.</span>
          <div className="out-problem-actions">
            <button
              type="button"
              className="gold"
              onClick={(event) => {
                event.stopPropagation();
                agree();
              }}
            >
              Show screen
            </button>
          </div>
        </div>
      )}
      {url && near && !held && (
        <iframe
          ref={frame}
          key={`${url}:${attempt}`}
          src={url}
          title={output.name}
          sandbox="allow-scripts"
          // Putting something on the clipboard is the one thing a screen may be let do, and only if its server asked.
          allow={page?.clipboard && full ? "clipboard-write" : undefined}
          referrerPolicy="no-referrer"
        />
      )}
      {!held && stage !== "shown" && (
        <div className="out-veil" role="status">
          {stage === "opening" ? (
            <span>Opening {who}…</span>
          ) : (
            <>
              <b>{who} didn't open</b>
              <span>It may need the internet to start.</span>
              <button
                type="button"
                onClick={(event) => {
                  event.stopPropagation();
                  setAttempt((count) => count + 1);
                }}
              >
                Try again
              </button>
            </>
          )}
        </div>
      )}
      {asking && (
        <div className="out-ask" role="alertdialog" aria-label={asking.title}>
          <div>
            <b>{asking.title}</b>
            <span>{asking.detail}</span>
          </div>
          <button type="button" onClick={() => asking.settle(false)}>
            Not now
          </button>
          {asking.always && (
            <button
              type="button"
              onClick={() => {
                asking.always?.();
                asking.settle(true);
              }}
            >
              Always in this chat
            </button>
          )}
          <button type="button" className="gold" onClick={() => asking.settle(true)}>
            <Icon name="check" />
            {asking.allow}
          </button>
        </div>
      )}
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import type { ScreenOrigins, ToolScreen } from "./api";
import { Icon } from "./Icon";
import { useWorkspace } from "./Workspace";
import { labelOf } from "./outputBlocks";
import { asItWillRun, bridge, halfWritten, type Fed } from "./outputBridge";
import { useOutputs, type RenderProps } from "./outputContext";
import { useNear } from "./outputFiles";
import { screened } from "./outputFrame";
import { agreeScreen, screenAgreed, screenIdentity, screenReach, screenScope } from "./outputConsent";

type Page = ScreenOrigins & { html: string; clipboard?: boolean; identity?: string };
type Asking = {
  title: string;
  detail: string;
  allow: string;
  /** Remembers the yes, where that is offered. */
  always?: () => void;
  settle: (yes: boolean) => void;
};

// Screens whose page has been put on the shelf this sitting; each is written there once.
const KEPT = new Map<string, number>();
const MAX_CHAT_CACHES = 32;
const MAX_KEPT = 64;
function bounded<K, V>(map: Map<K, V>, key: K, value: V, limit = MAX_CHAT_CACHES) {
  map.delete(key);
  map.set(key, value);
  while (map.size > limit) map.delete(map.keys().next().value!);
}

/** Drop transient tool approvals/catalogues when the viewer's chat ends. */
export function forgetScreens(chat: string) {
  OFFERS.delete(chat);
  for (const key of ALWAYS.keys()) if (key.startsWith(`${chat}\0`)) ALWAYS.delete(key);
  for (const key of PAGES.keys()) if (key.startsWith(`${chat}\0`)) PAGES.delete(key);
  for (const key of KEPT.keys()) if (key.startsWith(`${chat}\0`)) KEPT.delete(key);
}
// How long a started screen is given to draw, and how long one may take to start at all.
const SETTLE = 1500;
const STUCK = 15_000;
// How often a screen is given the call the model is still writing.
const BEAT = 150;
// Tools the person said a server's screens may always run, per chat, for this sitting only.
const ALWAYS = new Map<string, Set<string>>();

const AGREED = "medha-screen-agreed";

type Offered = Pick<ToolScreen, "server" | "tool" | "resource" | "text_fields"> & { name: string };
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
    bounded(OFFERS, chat, known);
    const unknown = names.split(" ").filter((name) => name.length <= 256 && !known.asked.has(name)).slice(0, 512);
    if (!unknown.length) return;
    unknown.forEach((name) => known.asked.add(name));
    while (known.asked.size > 512) known.asked.delete(known.asked.values().next().value!);
    void api
      .liveCall(chat, "mcp.screens")
      .then((answer) => {
        // The answer is this chat's whole screen list, so it is kept even when
        // more calls began meanwhile; those names are already marked asked and
        // would not ask again. Only a chat that has since ended is not refilled.
        if (OFFERS.get(chat) !== known) return;
        known.tools = ((answer.tools as Offered[] | undefined) ?? []).filter((tool) => [tool.name, tool.server, tool.tool, tool.resource].every((part) => typeof part === "string" && part.length <= 1024)).slice(0, 512);
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
      const key = `${chat}\0${server}\0${resource}`;
      if (PAGES.get(key)?.page === page && performance.now() - (KEPT.get(key) ?? -Infinity) > PAGE_FOR) {
        bounded(KEPT, key, performance.now(), MAX_KEPT);
        void api.screenKeep(server, resource, read).catch(() => KEPT.delete(key));
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
  // A screen calls back only the connection it was drawn by. One recorded before
  // connections were told apart calls the server of that name, as it always did.
  const sameServer = !screen.identity || screen.identity === page?.identity;
  const box = useRef<HTMLDivElement>(null);
  const frame = useRef<HTMLIFrameElement>(null);
  const near = useNear(box, false);
  const [url, setUrl] = useState<string>();
  const [asks, setAsks] = useState<Asking[]>([]);
  const outstanding = useRef<Asking[]>([]);
  outstanding.current = asks;
  // Whether the page in the frame has drawn yet; until it has, the frame is covered, not left blank.
  const [stage, setStage] = useState<"opening" | "shown" | "stuck">("opening");
  const [attempt, setAttempt] = useState(0);
  const full = size === "full";
  const who = labelOf(output);
  const [remember, setRemember] = useState(false);

  useEffect(() => {
    if (!page) return;
    const reach = screenReach(page);
    onDetail?.({
      foot: reach
        ? `${reach}. It can't see your files or your other chats.`
        : "Runs here, offline. It can't see your files or your other chats.",
    });
  }, [page, onDetail]);

  // A screen that reaches the web waits for one yes per server, remembered with
  // exactly what was agreed to: a server that later asks for more asks again.
  const wanted = page ? screenScope(page) : "";
  const identity = page ? screenIdentity(screen.server, screen.resource, page) : "";
  const [, setAgreed] = useState(0);
  const reaches = page && (page.connect.length + page.resources.length + page.frames.length > 0 || page.clipboard);
  const held = Boolean(reaches) && !screenAgreed(identity, wanted);
  const agree = () => {
    agreeScreen(identity, wanted, remember && Boolean(page?.identity));
    window.dispatchEvent(new Event(AGREED));
  };
  // The same screen is shown in the chat and beside it; a yes given in one is a yes in both.
  useEffect(() => {
    const heard = () => setAgreed((revision) => revision + 1);
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
        if (outstanding.current.length >= 16) return reject(new Error("This screen has too many pending requests. Try again after answering one."));
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
        outstanding.current = [...outstanding.current, mine];
        setAsks((list) => [...list, mine]);
      }),
    [full],
  );
  const asking = asks[0];
  useEffect(() => () => { for (const question of outstanding.current) question.settle(false); }, []);

  const context = useRef({ theme, full });
  context.current = { theme, full };
  const link = useRef<ReturnType<typeof bridge>>(undefined);
  const drawn = useRef<Fed>({ input: screen.input, result: screen.result });
  const fedAt = useRef(0);
  const { server } = screen;
  useEffect(() => {
    if (!url || !near) return;
    let current = true;
    let settling: ReturnType<typeof setTimeout> | undefined;
    const chat = scope.current.chatKey;
    const call = (method: "mcp.screen" | "mcp.screen.call", params: Record<string, unknown>) => {
      if (!current || !chat) return Promise.reject(new Error("This screen is no longer open."));
      if (!sameServer) return Promise.reject(new Error(`${who} has changed since this was made. It is shown as it was recorded.`));
      return api.liveCall(chat, method, { ...params, identity: screen.identity ?? page?.identity });
    };
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
          const room = `${chat}\0${server}\0${identity}`;
          if (!ALWAYS.get(room)?.has(name))
            await confirm(
              `${who} wants to run ${name}`,
              `It will run ${name} on the ${who} server.`,
              "Allow once",
              () => { const tools = (ALWAYS.get(room) ?? new Set<string>()).add(name); while (tools.size > 64) tools.delete(tools.values().next().value!); bounded(ALWAYS, room, tools); },
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
      current = false;
      made.close();
      for (const question of outstanding.current) question.settle(false);
      outstanding.current = [];
      clearTimeout(settling);
      clearTimeout(giveUp);
      window.removeEventListener("message", hear);
      link.current = undefined;
    };
  }, [api, scope, url, near, attempt, server, who, confirm, ask, screen.identity, page?.identity, sameServer, output.anchor, identity]);
  useEffect(() => {
    link.current?.changed({ theme: theme === "light" ? "light" : "dark" });
  }, [theme]);

  // What there is to draw reaches the screen as it grows: pieces of the call
  // while the model writes it, read on a beat, then the whole call, then its result.
  const { partial, input, result, text_fields: textFields } = screen;
  useEffect(() => {
    const feed = () => {
      fedAt.current = performance.now();
      drawn.current = { partial: partial === undefined ? undefined : asItWillRun(halfWritten(partial), textFields), input, result };
      link.current?.feed(drawn.current);
    };
    if (input !== undefined || partial === undefined) return feed();
    // The wait is what is left of the beat, so pieces arriving faster than it never put it off.
    const beat = setTimeout(feed, Math.max(0, BEAT - (performance.now() - fedAt.current)));
    return () => clearTimeout(beat);
  }, [partial, input, result, textFields]);

  return (
    <div className={`out-app ${full ? "live" : ""}`} ref={box}>
      {held && (
        <div className="out-problem">
          <Icon name="globe" />
          <b>{who} shows its own screen here</b>
          <span>{page && screenReach(page)}. It can't see your files or your other chats.</span>
          {page?.identity && <label><input type="checkbox" checked={remember} onChange={(event) => setRemember(event.target.checked)} />Remember these exact permissions for 90 days</label>}
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
          key={`${url}:${attempt}:${output.anchor}`}
          src={url}
          title={output.name}
          sandbox="allow-scripts"
          // Putting something on the clipboard is the one thing a screen may be let do, and only if its server asked.
          allow={page?.clipboard && full ? "clipboard-write" : undefined}
          referrerPolicy="no-referrer"        />
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

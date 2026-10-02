import { useWorkspace, useWorkspaceApi } from "./Workspace";
import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  type Attachment,
  type HistoryEvent,
  type Session,
  type SessionSettings,
} from "./api";
import { FileViewer } from "./FileViewer";
import { Extensions } from "./Extensions";
import { Rewind } from "./Rewind";
import { Settings } from "./Settings";
import { Composer, type Control } from "./Composer";
import { Icon } from "./Icon";
import { LiveTail } from "./LiveTail";
import { Palette, type Action } from "./Palette";
import { medhaIcon, Sidebar } from "./Sidebar";
import { WorkSurface, type SurfaceTab } from "./WorkSurface";
import { OutputScope } from "./OutputScope";
import { toBlocks, type SubagentRun } from "./timeline";
import { Transcript } from "./Transcript";
import { useLive } from "./useLive";
import { Starters, Welcome } from "./Welcome";
import { WorkspaceMenu } from "./WorkspaceMenu";

const TerminalDrawer = lazy(() =>
  import("./TerminalDrawer").then((module) => ({
    default: module.TerminalDrawer,
  })),
);

function stored(key: string) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function store(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* storage can be unavailable */
  }
}

export function App() {
  const api = useWorkspaceApi();
  const context = useWorkspace();
  const appRoot = useRef<HTMLDivElement>(null);
  const { theme, setTheme } = context;
  const [page, setPage] = useState<"chat" | "extensions" | "settings">("chat");
  const [rewind, setRewind] = useState<{ at?: string } | null>(null);
  const [preview, setPreview] = useState<string | null>(null);
  const [sidebarPeek, setSidebarPeek] = useState(false);
  const peekTimer = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  function peek() {
    if (peekTimer.current) clearTimeout(peekTimer.current);
    if (!sidebarOpen) setSidebarPeek(true);
  }
  function leaveSidebar() {
    if (peekTimer.current) clearTimeout(peekTimer.current);
    peekTimer.current = setTimeout(() => {
      if (!appRoot.current?.querySelector(".side:focus-within"))
        setSidebarPeek(false);
    }, 240);
  }
  const [workspace, setWorkspace] = useState("");
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [draftKey, setDraftKey] = useState<string | null>(
    () => `draft-${crypto.randomUUID()}`,
  );
  // Nothing sent from it yet: "New session" returns here instead of piling up empty chats.
  const spareDraft = useRef(draftKey);
  const [events, setEvents] = useState<HistoryEvent[]>([]);
  const [reload, setReload] = useState(0);
  const [loading, setLoading] = useState(true);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pinned, setPinned] = useState<Set<string>>(new Set());
  const [run, setRun] = useState<SubagentRun | null>(null);
  const text = useRef<Record<string, string>>({});
  const [draftRevision, setDraftRevision] = useState(0);
  const setText = useCallback(
    (change: (drafts: Record<string, string>) => Record<string, string>) => {
      text.current = change(text.current);
      setDraftRevision((revision) => revision + 1);
    },
    [],
  );
  const [keyBySession, setKeyBySession] = useState<Record<string, string>>({});
  const [away, updateAway] = useState(false);
  const awayRef = useRef(false);
  const setAway = useCallback((value: boolean) => {
    awayRef.current = value;
    updateAway(value);
  }, []);
  const [palette, setPalette] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(() =>
    stored("medha-sidebar")
      ? stored("medha-sidebar") === "open"
      : window.innerWidth > 780,
  );
  const [surface, setSurface] = useState<SurfaceTab | null>(null);
  const [terminalOpen, setTerminalOpen] = useState(false);
  const [terminalUsed, setTerminalUsed] = useState(false);
  const toggleTerminal = () => {
    setTerminalUsed(true);
    setTerminalOpen((open) => !open);
  };
  const [control, setControl] = useState<Control | null>(null);
  const [defaults, setDefaults] = useState<SessionSettings>();
  const [showReasoning, setShowReasoning] = useState(
    () => stored("medha-show-thinking") === "true",
  );
  const [attachments, setAttachments] = useState<Record<string, Attachment[]>>(
    {},
  );
  const [skills, setSkills] = useState<{ name: string; description: string }[]>([]);
  const [skillFor, setSkillFor] = useState<Record<string, { name: string; message: string }>>({});
  useEffect(() => {
    let active = true;
    const load = () =>
      void api
        .extensions()
        .then((catalog) => {
          if (!active) return;
          const rows = (catalog.skills as { name: string; description: string; enabled: boolean }[]) ?? [];
          setSkills(rows.filter((skill) => skill.enabled).map(({ name, description }) => ({ name, description })));
        })
        .catch(() => {});
    load();
    window.addEventListener("medha-preferences", load);
    return () => {
      active = false;
      window.removeEventListener("medha-preferences", load);
    };
  }, [api]);
  const [branch, setBranch] = useState<string | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const following = useRef(false);
  const lastTop = useRef(0);
  const settled = useRef<Record<string, number>>({});
  const {
    live,
    send,
    cancel,
    approve,
    clearTail,
    takeReturned,
    ensureOpen,
    configure,
    answer,
  } = useLive();

  useEffect(() => {
    store("medha-sidebar", sidebarOpen ? "open" : "closed");
  }, [sidebarOpen]);
  useEffect(() => {
    store("medha-show-thinking", String(showReasoning));
  }, [showReasoning]);

  useEffect(() => {
    if (workspace)
      setPinned(
        new Set(
          JSON.parse(stored(`medha-pins:${workspace}`) || "[]") as string[],
        ),
      );
  }, [workspace]);

  const { roots, subagentCounts } = useMemo(() => {
    const ids = new Set(sessions.map((session) => session.id));
    const counts = new Map<string, number>();
    for (const session of sessions) {
      if (session.parent_id && ids.has(session.parent_id))
        counts.set(session.parent_id, (counts.get(session.parent_id) ?? 0) + 1);
    }
    return {
      roots: sessions.filter(
        (session) => !session.parent_id || !ids.has(session.parent_id),
      ),
      subagentCounts: counts,
    };
  }, [sessions]);

  const refresh = useCallback(async (quiet = false, select?: string) => {
    if (!quiet) setLoading(true);
    setError(null);
    try {
      const [path, rows, head, settings] = await Promise.all([
        api.workspace(),
        api.sessions(),
        api.branch().catch(() => null),
        api.defaults().catch(() => undefined),
      ]);
      setDefaults(settings);
      setWorkspace(path);
      setBranch(head);
      setSessions(rows);
      const ids = new Set(rows.map((session) => session.id));
      if (select && ids.has(select)) {
        setDraftKey(null);
        setSelected(select);
      } else if (!quiet) {
        setSelected((old) => (old && ids.has(old) ? old : null));
      }
    } catch (cause) {
      setError(String(cause));
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const keyFor = useCallback(
    (id: string) => keyBySession[id] ?? id,
    [keyBySession],
  );
  const currentKey = draftKey ?? (selected ? keyFor(selected) : null);
  context.scope.current = {
    chatKey: currentKey,
    sessionId: draftKey ? null : selected,
  };
  useEffect(() => {
    void api.liveFocus(currentKey).catch(() => undefined);
  }, [api, currentKey]);
  useEffect(() => {
    let active = true;
    void api
      .workspace()
      .then((path) => active && setWorkspace(path))
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [currentKey, selected]);
  useEffect(() => {
    const open = (event: Event) => {
      const detail = (event as CustomEvent<{ workspace: string; path: string }>)
        .detail;
      if (detail.workspace === context.workspace.id && context.active)
        setPreview(detail.path);
    };
    window.addEventListener("medha-file", open);
    return () => window.removeEventListener("medha-file", open);
  }, [context.active]);
  const [focusChange, setFocusChange] = useState<{
    path: string;
    at: number;
  }>();
  useEffect(() => {
    const show = (event: Event) => {
      const detail = (event as CustomEvent<{ workspace: string; path: string }>)
        .detail;
      if (detail.workspace !== context.workspace.id || !context.active) return;
      setSurface("changes");
      setFocusChange({ path: detail.path, at: Date.now() });
    };
    window.addEventListener("medha-change", show);
    return () => window.removeEventListener("medha-change", show);
  }, [context.active]);
  useEffect(
    () => () => {
      if (peekTimer.current) clearTimeout(peekTimer.current);
    },
    [],
  );

  const current = draftKey
    ? undefined
    : roots.find((session) => session.id === selected);
  const state = currentKey ? live[currentKey] : undefined;
  const working =
    state?.status === "running" ||
    (state?.status === "starting" && state.sent.length > 0);
  const toggleRun = useCallback((next: SubagentRun) => {
    setSurface("agents");
    setRun((open) => (open?.childId === next.childId ? null : next));
  }, []);

  useEffect(() => {
    setRun(null);
    setPreview(null);
    setRewind(null);
    setControl(null);
    setAway(false);
    setEvents([]);
  }, [selected, draftKey]);

  useEffect(() => {
    if (!selected || draftKey) return;
    let active = true;
    setHistoryLoading(true);
    (async () => {
      let all: HistoryEvent[] = [];
      let cursor: string | null = null;
      do {
        const page = await api.events(selected, cursor);
        if (!active) return;
        all = [...all, ...page.events];
        cursor = page.next_cursor;
        setEvents(all);
      } while (cursor);
      clearTail(
        keyFor(selected),
        new Set(
          all
            .filter((event) => event.kind === "user")
            .map((event) => event.text ?? ""),
        ),
      );
    })()
      .catch((cause) => active && setError(String(cause)))
      .finally(() => active && setHistoryLoading(false));
    return () => {
      active = false;
    };
  }, [selected, draftKey, reload]);

  useEffect(() => {
    for (const [key, entry] of Object.entries(live)) {
      if (
        entry.sessionId &&
        !keyBySession[entry.sessionId] &&
        key !== entry.sessionId
      ) {
        setKeyBySession((map) => ({ ...map, [entry.sessionId!]: key }));
      }
      if (entry.settled === (settled.current[key] ?? 0)) continue;
      settled.current[key] = entry.settled;
      if (
        key === currentKey &&
        entry.sessionId &&
        entry.sessionId !== selected
      ) {
        void refresh(true, entry.sessionId);
      } else if (key === draftKey && entry.sessionId) {
        void refresh(true, entry.sessionId);
      } else {
        void refresh(true);
        if (key === currentKey) setReload((value) => value + 1);
      }
    }
  }, [live, keyBySession, draftKey, currentKey, refresh]);

  // A stopped turn hands back what it never read; typed text must not vanish.
  useEffect(() => {
    for (const [key, entry] of Object.entries(live)) {
      if (!entry.returned?.length) continue;
      const restored = takeReturned(key).join("\n\n");
      setText((all) => ({
        ...all,
        [key]: all[key] ? `${restored}\n\n${all[key]}` : restored,
      }));
    }
  }, [live, takeReturned, setText]);

  useEffect(() => {
    const box = scroller.current;
    if (!box || awayRef.current) return;
    const frame = requestAnimationFrame(() => {
      if (awayRef.current) return;
      const bottom = Math.max(0, box.scrollHeight - box.clientHeight);
      if (Math.abs(box.scrollTop - bottom) < 1) return;
      following.current = true;
      box.scrollTop = bottom;
      lastTop.current = box.scrollTop;
    });
    return () => cancelAnimationFrame(frame);
  }, [
    events.length,
    state?.items,
    state?.sent.length,
    state?.approvals.length,
    state?.error,
    away,
  ]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!context.active) return;
      const mod = event.metaKey || event.ctrlKey;
      if (event.ctrlKey && event.code === "Backquote") {
        event.preventDefault();
        toggleTerminal();
        return;
      }
      // Preserve shell editing keys, Ctrl+C, and Escape inside the terminal.
      if (
        (event.target as HTMLElement).closest(".terminal-canvas") &&
        !event.metaKey
      )
        return;
      if (event.key === "Escape") {
        setRun(null);
        setPreview(null);
        setRewind(null);
        setSidebarPeek(false);
        setSurface(null);
        if (window.innerWidth <= 780) setSidebarOpen(false);
      }
      if (mod && event.key.toLowerCase() === "b") {
        event.preventDefault();
        setSidebarOpen((open) => !open);
      }
      if (mod && event.key === ".") {
        event.preventDefault();
        setSurface((tab) => (tab ? null : "agents"));
      }
      if (mod && event.key.toLowerCase() === "n") {
        event.preventDefault();
        startNew();
      }
      if (mod && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPalette((open) => !open);
      }
      if (mod && event.shiftKey && event.key.toLowerCase() === "l") {
        event.preventDefault();
        setTheme(theme === "dark" ? "light" : "dark");
      }
    };

    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, [context.active, theme]);

  function spare() {
    spareDraft.current ??= `draft-${crypto.randomUUID()}`;
    return spareDraft.current;
  }

  function startNew() {
    setPage("chat");
    setPreview(null);
    setDraftKey(spare());
  }

  /** A new chat with its first message written but not sent. */
  function startWith(prompt: string) {
    setPage("chat");
    setPreview(null);
    const key = spare();
    setDraftKey(key);
    setText((all) => ({ ...all, [key]: prompt }));
  }

  function submit(
    message = currentKey ? (text.current[currentKey] ?? "") : "",
  ) {
    const images = currentKey ? (attachments[currentKey] ?? []) : [];
    const typed =
      message.trim() || (images.length ? "Describe the attached images." : "");
    if (!currentKey || !typed) return;
    // The request leads; the skill's procedure follows in the same words the
    // TUI uses, so the model sees the same thing on either surface.
    const skill = skillFor[currentKey];
    const body = skill ? `${typed}\n\n${skill.message}` : typed;
    if (skill) setSkillFor(({ [currentKey]: _, ...rest }) => rest);
    setText((all) => ({ ...all, [currentKey]: "" }));
    setAway(false);
    setAttachments((all) => ({ ...all, [currentKey]: [] }));
    if (currentKey === spareDraft.current) spareDraft.current = null;
    void send(currentKey, draftKey ? null : selected, body, images).catch(
      () => {
        setText((all) => ({
          ...all,
          [currentKey]: all[currentKey] || message,
        }));
        setAttachments((all) => ({
          ...all,
          [currentKey]: all[currentKey]?.length ? all[currentKey] : images,
        }));
      },
    );
  }

  function togglePin(id: string) {
    setPinned((previous) => {
      const next = new Set(previous);
      if (!next.delete(id)) next.add(id);
      store(`medha-pins:${workspace}`, JSON.stringify([...next]));
      return next;
    });
  }

  const blocks = useMemo(() => toBlocks(events), [events]);
  const folder = context.workspace.name;
  const liveSessions = Object.values(live).filter((entry) => entry.sessionId);
  const runningSessions = new Set(
    liveSessions
      .filter((entry) => entry.status === "running")
      .map((entry) => entry.sessionId!),
  );
  const needsSessions = new Set(
    liveSessions
      .filter(
        (entry) => entry.approvals.length > 0 || entry.questions.length > 0,
      )
      .map((entry) => entry.sessionId!),
  );
  const waiting = Boolean(state?.approvals.length || state?.questions.length);
  const settingsLocked = Boolean(
    working || state?.agents.some((agent) => agent.status === "running"),
  );
  const settings = state?.settings ?? defaults;
  const openControl = (next: Control) => {
    if (!currentKey) startNew();
    setControl(next);
  };
  const openSession = (id: string) => {
    setPage("chat");
    setPreview(null);
    setSidebarPeek(false);
    setDraftKey(null);
    setSelected(id);
    if (window.innerWidth <= 780) setSidebarOpen(false);
  };
  const openRewind = useCallback(
    (at?: string) => {
      if (!settingsLocked && currentKey) setRewind({ at });
    },
    [settingsLocked, currentKey],
  );
  const answerCurrent = useCallback(
    (
      form: Parameters<typeof answer>[1],
      answers?: Parameters<typeof answer>[2],
    ) => {
      if (currentKey) answer(currentKey, form, answers);
    },
    [currentKey, answer],
  );
  const approveCurrent = useCallback(
    (approval: Parameters<typeof approve>[1], allow: boolean) => {
      if (currentKey) approve(currentKey, approval, allow);
    },
    [currentKey, approve],
  );
  const openAgent = useCallback(
    (agent: NonNullable<typeof state>["agents"][number]) => {
      toggleRun({
        id: agent.session,
        name: agent.name,
        childId: agent.session,
        objective: agent.objective,
      });
    },
    [toggleRun],
  );
  const preferenceReload = useRef(new Set<string>());
  useEffect(() => {
    const update = () => {
      Object.keys(live).forEach((key) => preferenceReload.current.add(key));
      void refresh(true);
    };
    window.addEventListener("medha-preferences", update);
    return () => window.removeEventListener("medha-preferences", update);
  }, [live, refresh]);
  useEffect(() => {
    for (const key of preferenceReload.current) {
      const entry = live[key];
      if (
        !entry ||
        entry.status !== "idle" ||
        entry.agents.some((agent) => agent.status === "running")
      )
        continue;
      preferenceReload.current.delete(key);
      void api
        .liveCall(key, "extensions.reload")
        .catch((cause) => setError(String(cause)));
    }
  }, [live, defaults]);
  const actions: Action[] = [
    ...(currentKey && !settingsLocked
      ? [
          {
            id: "rewind",
            icon: "rewind" as const,
            label: "Rewind conversation or files",
            run: () => openRewind(),
          },
          {
            id: "clear",
            icon: "chat" as const,
            label: "Clear conversation · keep history",
            run: startNew,
          },
        ]
      : []),
    {
      id: "extensions",
      icon: "blocks",
      label: "Extensions",
      run: () => setPage("extensions"),
    },
    {
      id: "settings",
      icon: "settings",
      label: "Settings",
      run: () => setPage("settings"),
    },
    {
      id: "files",
      icon: "folder",
      label: "Browse files",
      run: () => setSurface("files"),
    },
    {
      id: "terminal",
      icon: "term",
      label: terminalOpen ? "Hide terminal" : "Show terminal",
      hint: "⌃`",
      run: toggleTerminal,
    },
    {
      id: "model",
      icon: "cpu",
      label: "Choose model",
      run: () => openControl("model"),
    },
    {
      id: "reasoning",
      icon: "brain",
      label: "Configure thinking",
      run: () => openControl("reasoning"),
    },
    {
      id: "mode",
      icon: "shield",
      label: "Change execution mode",
      run: () => openControl("mode"),
    },
    {
      id: "agents",
      icon: "fork",
      label: "Inspect agents",
      run: () => setSurface("agents"),
    },
    {
      id: "changes",
      icon: "branch",
      label: "Review changes",
      run: () => setSurface("changes"),
    },
    {
      id: "context",
      icon: "list",
      label: "Context and usage",
      run: () => setSurface("context"),
    },
    {
      id: "sidebar",
      icon: "panel",
      label: sidebarOpen ? "Hide sessions" : "Show sessions",
      hint: "⌘B",
      run: () => setSidebarOpen(!sidebarOpen),
    },
    {
      id: "new",
      icon: "plus",
      label: "New session",
      hint: "⌘N",
      run: startNew,
    },
    {
      id: "theme",
      icon: theme === "dark" ? "sun" : "moon",
      label: theme === "dark" ? "Switch to parchment" : "Switch to ink",
      hint: "⇧⌘L",
      run: () => setTheme(theme === "dark" ? "light" : "dark"),
    },
    {
      id: "refresh",
      icon: "refresh",
      label: "Refresh sessions",
      run: () => void refresh(),
    },
    ...(working && currentKey
      ? [
          {
            id: "stop",
            icon: "x" as const,
            label: "Stop this turn",
            run: () => cancel(currentKey),
          },
        ]
      : []),
  ];
  useEffect(() => {
    if (palette) setControl(null);
  }, [palette]);
  const fresh =
    draftKey && (!state || state.sent.length === 0) && !state?.error;
  // A new session's own screen: the composer in the middle of the page, not at its foot.
  const home = Boolean(fresh) && page === "chat";

  return (
    <OutputScope
      chat={currentKey}
      viewing={surface === "preview" && !preview}
      onShow={() => {
        setPreview(null);
        setSurface("preview");
      }}
      onMade={() => {
        if (preview || (surface && surface !== "preview")) return false;
        setSurface("preview");
        return true;
      }}
      onAsk={(request) => {
        if (!currentKey) return;
        setText((all) => ({
          ...all,
          [currentKey]: [all[currentKey], request].filter(Boolean).join("\n\n"),
        }));
      }}
    >
    <div
      ref={appRoot}
      className={`shell ${surface || preview ? "with-panel" : ""} ${surface === "preview" && !preview ? "previewing" : ""}${sidebarOpen ? "sidebar-open" : "sidebar-hidden"} ${sidebarPeek && !sidebarOpen ? "sidebar-peek" : ""}`}
    >
      {!sidebarOpen && (
        <div
          className="sidebar-hover-edge"
          onPointerEnter={peek}
          aria-hidden="true"
        />
      )}
      {sidebarOpen && (
        <button
          type="button"
          className="sidebar-scrim"
          onClick={() => setSidebarOpen(false)}
          aria-label="Close sessions"
        />
      )}
      <Sidebar
        sessions={roots}
        subagentCounts={subagentCounts}
        selected={draftKey ? null : selected}
        onSelect={openSession}
        onNew={startNew}
        onPalette={() => setPalette(true)}
        drafting={Boolean(draftKey)}
        running={runningSessions}
        needs={needsSessions}
        pinned={pinned}
        onTogglePin={togglePin}
        onRefresh={() => void refresh()}
        loading={loading}
        workspace={workspace}
        theme={theme}
        visible={sidebarOpen || sidebarPeek}
        onPointerEnter={peek}
        onPointerLeave={leaveSidebar}
        onExtensions={() => setPage("extensions")}
        onSettings={() => setPage("settings")}
        page={page}
        workspaceName={folder}
        onToggleTheme={() => setTheme(theme === "dark" ? "light" : "dark")}
      />

      <main className={home ? "main home" : "main"}>
        <header className="head">
          <button
            type="button"
            className="icon-btn sidebar-toggle"
            onClick={() => {
              setSidebarOpen(!sidebarOpen);
              setSidebarPeek(false);
            }}
            onPointerEnter={peek}
            onPointerLeave={leaveSidebar}
            aria-expanded={sidebarOpen}
            aria-controls="session-sidebar"
            aria-label={sidebarOpen ? "Hide sessions" : "Show sessions"}
            title={sidebarOpen ? "Hide sessions · ⌘B" : "Show sessions · ⌘B"}
          >
            <Icon name="panel" />
          </button>
          <div className="head-title">
            <h1>
              {page === "extensions"
                ? "Extensions"
                : page === "settings"
                  ? "Settings"
                  : draftKey
                    ? "New session"
                    : current?.title || folder}
            </h1>
            <span className="crumb">
              <WorkspaceMenu folder={folder} workspace={workspace} />
              {branch && (
                <>
                  <Icon name="branch" />
                  {branch}
                </>
              )}
            </span>
          </div>
          {waiting ? (
            <span className="run-pill await">
              <span className="need-dot" />
              Needs your input
            </span>
          ) : (
            working && (
              <span className="run-pill">
                <span className="spin-dot" />
                Working
              </span>
            )
          )}
          <div className="head-tools">
            <button
              type="button"
              className={`icon-btn ${terminalOpen ? "active" : ""}`}
              onClick={toggleTerminal}
              aria-pressed={terminalOpen}
              aria-label="Toggle terminal"
              title="Terminal · ⌃`"
            >
              <Icon name="term" />
            </button>
            <button
              type="button"
              className={`icon-btn surface-toggle ${surface ? "active" : ""}`}
              onClick={() => setSurface(surface ? null : "agents")}
              aria-pressed={Boolean(surface)}
              aria-label="Toggle work surface"
              title="Work surface · ⌘."
            >
              <Icon name="panel" />
            </button>
          </div>
        </header>

        {error && (
          <div className="error" role="alert">
            <b>Medha couldn’t load this</b>
            <span>{error}</span>
            <button
              type="button"
              className="btn-line"
              onClick={() => void refresh()}
            >
              Try again
            </button>
          </div>
        )}

        {page === "extensions" && (
          <Extensions
            sessionKey={currentKey}
            ensureOpen={async () => {
              if (currentKey)
                await ensureOpen(currentKey, draftKey ? null : selected);
            }}
            locked={settingsLocked}
            onAsk={startWith}
          />
        )}
        {page === "settings" && (
          <Settings
            sessionKey={currentKey}
            ensureOpen={async () => {
              if (currentKey)
                await ensureOpen(currentKey, draftKey ? null : selected);
            }}
          />
        )}
        <div
          hidden={page !== "chat"}
          className="scroll"
          ref={scroller}
          onWheel={(event) => {
            if (event.deltaY < 0) {
              following.current = false;
              setAway(true);
            }
          }}
          onScroll={(event) => {
            const box = event.currentTarget;
            const up = box.scrollTop < lastTop.current - 2;
            lastTop.current = box.scrollTop;
            const ours = following.current;
            following.current = false;
            if (up && !ours) setAway(true);
            else if (box.scrollHeight - box.scrollTop - box.clientHeight < 32)
              setAway(false);
          }}
        >
          <div className="thread">
            {fresh && (
              <Welcome folder={folder} branch={branch} />
            )}
            {!draftKey && loading && !current && (
              <div className="empty">
                <img src={medhaIcon} alt="" />
                <p>Opening {folder}…</p>
              </div>
            )}
            {!draftKey && !loading && !current && !error && (
              <div className="empty">
                <img src={medhaIcon} alt="" />
                <h2>No sessions yet</h2>
                <p>
                  Start a new session to give Medha its first task in {folder}.
                </p>
                <button type="button" className="btn-gold" onClick={startNew}>
                  New session
                </button>
              </div>
            )}
            {current && (
              <>
                <div className="day">
                  {new Date(current.started_ts * 1000).toLocaleDateString(
                    undefined,
                    { weekday: "long", month: "long", day: "numeric" },
                  )}
                </div>
                <Transcript
                  blocks={blocks}
                  showReasoning={showReasoning}
                  openRun={run?.childId}
                  onOpenRun={toggleRun}
                  liveAgents={state?.agents}
                  onRewind={settingsLocked ? undefined : openRewind}
                />
                {historyLoading && (
                  <p className="quiet">Reading the session…</p>
                )}
                {!historyLoading &&
                  events.length === 0 &&
                  !state?.sent.length && (
                    <p className="quiet">
                      This session has no messages to show.
                    </p>
                  )}
              </>
            )}
            {state && currentKey && !fresh && (
              <LiveTail
                state={state}
                showReasoning={showReasoning}
                onAnswer={answerCurrent}
                onApprove={approveCurrent}
                openSession={run?.childId}
                onOpenAgent={openAgent}
              />
            )}
          </div>
        </div>

        {away && working && (
          <button type="button" className="jump" onClick={() => setAway(false)}>
            Jump to latest
          </button>
        )}

        {page === "chat" && (current || draftKey) && (
          <Composer
            key={currentKey}
            value={currentKey ? (text.current[currentKey] ?? "") : ""}
            resetRevision={draftRevision}
            onChange={(value) => {
              if (currentKey) text.current[currentKey] = value;
            }}
            onSend={() => submit()}
            onStop={() => currentKey && cancel(currentKey)}
            running={working}
            disabled={!currentKey}
            placeholder={
              draftKey ? "Describe a task for Medha" : "Continue this session"
            }
            model={state?.model}
            settings={settings}
            settingsLocked={settingsLocked}
            control={control}
            onControl={setControl}
            onLoadSettings={async () => {
              if (currentKey)
                await ensureOpen(currentKey, draftKey ? null : selected);
            }}
            onConfigure={async (change) => {
              if (currentKey)
                await configure(currentKey, draftKey ? null : selected, change);
            }}
            showReasoning={showReasoning}
            onShowReasoning={setShowReasoning}
            attachments={currentKey ? (attachments[currentKey] ?? []) : []}
            onAttachments={(next) =>
              currentKey &&
              setAttachments((all) => ({ ...all, [currentKey]: next }))
            }
            onSurface={setSurface}
            onNew={startNew}
            onRewind={() => openRewind()}
            onSettings={() => setPage("settings")}
            onExtensions={() => setPage("extensions")}
            contextPercent={state?.contextPercent}
            skills={skills}
            skill={currentKey ? skillFor[currentKey]?.name : undefined}
            onSkill={(name) => {
              if (!currentKey) return;
              const key = currentKey;
              if (!name) {
                setSkillFor(({ [key]: _, ...rest }) => rest);
                return;
              }
              return api
                .extensions("extensions.skill.use", { name })
                .then((loaded) => setSkillFor((all) => ({ ...all, [key]: { name, message: String(loaded.message) } })));
            }}
          />
        )}
        {home && <Starters folder={folder} onPick={(starter) => submit(starter)} />}
        {terminalUsed && (
          <Suspense fallback={<p className="quiet">Opening terminal…</p>}>
            <TerminalDrawer
              open={terminalOpen}
              workspace={workspace}
              onHide={() => {
                setTerminalOpen(false);
                appRoot.current
                  ?.querySelector<HTMLTextAreaElement>(".composer textarea")
                  ?.focus();
              }}
            />
          </Suspense>
        )}
      </main>

      {preview ? (
        <FileViewer path={preview} onClose={() => setPreview(null)} />
      ) : (
        surface && (
          <WorkSurface
            tab={surface}
            onTab={setSurface}
            onClose={() => {
              setSurface(null);
              setRun(null);
            }}
            state={state}
            settings={settings}
            blocks={blocks}
            run={run}
            onAgent={setRun}
            showReasoning={showReasoning}
            sessionKey={currentKey}
            sessionId={state?.sessionId ?? (draftKey ? null : selected)}
            focusChange={focusChange}
            ensureOpen={async () => {
              if (currentKey)
                await ensureOpen(currentKey, draftKey ? null : selected);
            }}
            locked={settingsLocked}
            onFile={setPreview}
          />
        )
      )}

      {rewind && currentKey && (
        <Rewind
          sessionKey={currentKey}
          at={rewind.at}
          ensureOpen={async () => {
            await ensureOpen(currentKey, draftKey ? null : selected);
          }}
          onClose={() => setRewind(null)}
          onComplete={(result) => {
            if (!result.code_only) {
              setKeyBySession((previous) => {
                const next = { ...previous };
                delete next[result.source];
                next[result.session] = currentKey;
                return next;
              });
              setSelected(result.session);
              setDraftKey(null);
              setText((all) => ({ ...all, [currentKey]: result.prefill }));
              setAttachments((all) => ({
                ...all,
                [currentKey]: result.images,
              }));
            }
            setReload((value) => value + 1);
          }}
        />
      )}
      {palette && (
        <Palette
          actions={actions}
          sessions={roots}
          onOpen={openSession}
          onClose={() => setPalette(false)}
        />
      )}

      <footer className="status">
        <span className={`dot ${error ? "failed" : ""}`} />
        {error ? "Backend needs attention" : "Connected to medha"}

      </footer>
    </div>
    </OutputScope>
  );
}

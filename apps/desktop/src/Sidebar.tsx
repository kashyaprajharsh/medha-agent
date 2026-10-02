import type { Session } from "./api";
import { type ReactNode, useId, useState } from "react";
import { Icon } from "./Icon";

export const medhaIcon = new URL("./assets/medha-logo.png", import.meta.url)
  .href;

type Props = {
  onPointerEnter: () => void;
  onPointerLeave: () => void;
  onExtensions: () => void;
  onSettings: () => void;
  page: string;
  notice?: ReactNode;
  workspaceName: string;
  sessions: Session[];
  subagentCounts: Map<string, number>;
  selected: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onPalette: () => void;
  drafting: boolean;
  running: Set<string>;
  needs: Set<string>;
  pinned: Set<string>;
  onTogglePin: (id: string) => void;
  onRefresh: () => void;
  loading: boolean;
  workspace: string;
  theme: string;
  visible: boolean;
  onToggleTheme: () => void;
};

const DATE_GROUPS = ["Today", "This week", "This month", "Older"];
const GROUPS = ["Needs you", "Running", "Pinned", ...DATE_GROUPS];

function dateGroup(ts: number, now: Date) {
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const week = new Date(today);
  week.setDate(week.getDate() - ((week.getDay() + 6) % 7));
  const month = new Date(now.getFullYear(), now.getMonth(), 1);
  const time = ts * 1000;
  if (time >= today.getTime()) return "Today";
  if (time >= week.getTime()) return "This week";
  if (time >= month.getTime()) return "This month";
  return "Older";
}

function when(ts: number, now = new Date()) {
  const start = new Date(
    now.getFullYear(),
    now.getMonth(),
    now.getDate(),
  ).getTime();
  const date = new Date(ts * 1000);
  if (ts * 1000 >= start)
    return date.toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
    });
  const yesterday = new Date(start);
  yesterday.setDate(yesterday.getDate() - 1);
  if (ts * 1000 >= yesterday.getTime()) return "Yesterday";
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function Sidebar(props: Props) {
  const groupId = useId();
  const [expanded, setExpanded] = useState(() => new Set(["Today"]));
  const { pinned } = props;
  const now = new Date();
  const sessions = [...props.sessions].sort((a, b) => b.last_ts - a.last_ts);
  const groups = new Map<string, Session[]>(GROUPS.map((label) => [label, []]));
  for (const session of sessions) {
    const label = props.needs.has(session.id)
      ? "Needs you"
      : props.running.has(session.id)
        ? "Running"
        : pinned.has(session.id)
          ? "Pinned"
          : dateGroup(session.last_ts, now);
    groups.get(label)!.push(session);
  }
  for (const [label, rows] of groups) if (!rows.length) groups.delete(label);
  const folder =
    props.workspace.split(/[\\/]/).filter(Boolean).at(-1) || "Workspace";

  return (
    <aside
      id="session-sidebar"
      onPointerEnter={props.onPointerEnter}
      onPointerLeave={props.onPointerLeave}
      className="side"
      aria-label="Sessions and workspace"
      inert={!props.visible}
    >
      <div className="brand">
        <img src={medhaIcon} alt="" />
        <span>Medha</span>
      </div>
      <div className="side-actions">
        <button
          type="button"
          className={`side-btn primary ${props.drafting ? "active" : ""}`}
          onClick={props.onNew}
        >
          <Icon name="plus" />
          <span>New session</span>
          <kbd>⌘N</kbd>
        </button>
        <button type="button" className="side-btn" onClick={props.onPalette}>
          <Icon name="search" />
          <span>Search and commands</span>
          <kbd>⌘K</kbd>
        </button>
      </div>
      <div className="side-heading">
        <span>Sessions</span>
        <button
          type="button"
          className="icon-btn sm"
          onClick={props.onRefresh}
          aria-label="Refresh sessions"
          title="Refresh"
        >
          <Icon
            name="refresh"
            className={`icon ${props.loading ? "spinning" : ""}`}
          />
        </button>
      </div>
      <nav className="sessions" aria-label="Sessions">
        {[...groups].map(([label, rows]) => {
          const collapsible = DATE_GROUPS.includes(label);
          const open = !collapsible || expanded.has(label);
          const id = `${groupId}-${label.replaceAll(" ", "-")}`;
          return (
            <section key={label}>
              <h2 className={`group ${label === "Needs you" ? "need" : ""}`}>
                {collapsible ? (
                  <button
                    type="button"
                    className="group-toggle"
                    aria-expanded={open}
                    aria-controls={id}
                    onClick={() =>
                      setExpanded((previous) => {
                        const next = new Set(previous);
                        if (!next.delete(label)) next.add(label);
                        return next;
                      })
                    }
                  >
                    <span>{label}</span>
                    <span className="n">{rows.length}</span>
                    <Icon
                      name="chev"
                      className={`icon group-chevron ${open ? "open" : ""}`}
                    />
                  </button>
                ) : (
                  <>
                    {label}
                    <span className="n">{rows.length}</span>
                  </>
                )}
              </h2>
              <div id={id} hidden={!open}>
                {open &&
                  rows.map((session) => {
                    const agents = props.subagentCounts.get(session.id) ?? 0;
                    const active = props.selected === session.id;
                    const isPinned = pinned.has(session.id);
                    return (
                      <div className="sess-row" key={session.id}>
                        <button
                          type="button"
                          className={`sess ${active ? "active" : ""}`}
                          onClick={() => props.onSelect(session.id)}
                          aria-current={active ? "page" : undefined}
                        >
                          <span className="sess-title">
                            {session.title || "Untitled session"}
                          </span>
                          <span className="sess-when">
                            {props.needs.has(session.id) ? (
                              <span
                                className="need-dot"
                                aria-label="Waiting for you"
                              />
                            ) : props.running.has(session.id) ? (
                              <span className="spin-dot" aria-label="Working" />
                            ) : (
                              when(session.last_ts, now)
                            )}
                          </span>
                          {agents > 0 && (
                            <span className="sess-meta">
                              <Icon name="fork" />
                              {agents}{" "}
                              {agents === 1 ? "sub-agent" : "sub-agents"}
                            </span>
                          )}
                        </button>
                        <button
                          type="button"
                          className="pin"
                          aria-pressed={isPinned}
                          aria-label={`${isPinned ? "Unpin" : "Pin"} ${session.title || "session"}`}
                          title={isPinned ? "Unpin" : "Pin"}
                          onClick={() => props.onTogglePin(session.id)}
                        >
                          <Icon name="pin" />
                        </button>
                      </div>
                    );
                  })}
              </div>
            </section>
          );
        })}
        {!props.loading && sessions.length === 0 && (
          <p className="side-empty">
            No sessions in {folder} yet. Choose New session to get started.
          </p>
        )}
      </nav>
      {props.notice}
      <div className="side-sections">
        <button
          className={`side-btn ${props.page === "extensions" ? "active" : ""}`}
          onClick={props.onExtensions}
        >
          <Icon name="blocks" />
          Extensions
        </button>
        <button
          className={`side-btn ${props.page === "settings" ? "active" : ""}`}
          onClick={props.onSettings}
        >
          <Icon name="settings" />
          Settings
        </button>
      </div>
      <div className="side-foot">
        <div className="ws" title={props.workspace}>
          <span className="ws-mark" aria-hidden="true">
            <img src={medhaIcon} alt="" />
          </span>
          <span className="ws-text">
            <b>{props.workspaceName}</b>
            <small>{props.workspace || "Opening workspace…"}</small>
          </span>
        </div>
        <button
          type="button"
          className="icon-btn"
          onClick={props.onToggleTheme}
          aria-label="Switch theme"
          title="Switch theme"
        >
          <Icon name={props.theme === "dark" ? "sun" : "moon"} />
        </button>
      </div>
    </aside>
  );
}

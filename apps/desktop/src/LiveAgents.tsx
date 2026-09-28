import { useState } from "react";
import { Icon } from "./Icon";
import type { LiveAgent } from "./live";
import { Spinner } from "./Spinner";
import { useNow } from "./Steps";
import { doingLine, duration, tokens } from "./timeline";

type Props = {
  agents: LiveAgent[];
  openSession?: string | null;
  onOpen: (agent: LiveAgent) => void;
};

/** The agents working right now, pinned under the turn like the TUI's tree. */
export function LiveAgents({ agents, openSession, onOpen }: Props) {
  const [expanded, setExpanded] = useState(false);
  const running = agents.filter((agent) => agent.status === "running");
  const now = useNow(running.length > 0);
  if (running.length === 0) return null;
  const waiting = running.filter(
    (agent) => agent.doing?.state === "waiting",
  ).length;
  return (
    <section className="agents live" aria-label="Agents running">
      <button
        type="button"
        className="agents-head agents-disclosure"
        aria-expanded={expanded}
        onClick={() => setExpanded(!expanded)}
      >
        <Spinner />
        <b>
          {running.length} {running.length === 1 ? "agent" : "agents"} running
        </b>
        {waiting > 0 && (
          <span className="agents-waiting">{waiting} waiting on you</span>
        )}
        <Icon name="chev" className="icon chev" />
      </button>
      {expanded && (
        <ul>
          {agents.map((agent) => {
            const counts = [
              agent.toolCalls !== undefined
                ? `${agent.toolCalls} ${agent.toolCalls === 1 ? "tool" : "tools"}`
                : undefined,
              agent.tokens !== undefined ? tokens(agent.tokens) : undefined,
              agent.status === "running"
                ? duration(Math.max(0, now * 1000 - agent.startedMs))
                : undefined,
            ].filter(Boolean);
            const waits = agent.doing?.state === "waiting";
            return (
              <li key={agent.session}>
                <button
                  type="button"
                  className={`agent-row ${openSession === agent.session ? "active" : ""} ${waits ? "waits" : ""}`}
                  onClick={() => onOpen(agent)}
                >
                  <span className={`agent-dot ${agent.status}`} />
                  <span className="agent-main">
                    <b>{agent.name}</b>
                    <span>{doingLine(agent)}</span>
                  </span>
                  <span className="agent-meta">{counts.join(" · ")}</span>
                  <Icon name="chev" className="icon chev" />
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

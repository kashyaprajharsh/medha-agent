import { useEffect, useState } from "react";
import { Icon } from "./Icon";
import { Veena } from "./Veena";
import { useWorkspaceApi } from "./Workspace";

const STARTERS = [
  { icon: "branch", text: "Review my uncommitted changes before I commit" },
  { icon: "tool", text: "Explain how this project is structured" },
  { icon: "term", text: "Find the slowest tests in the workspace" },
] as const;

/** Where the person is, in one line: the folder, its branch, and what is uncommitted. */
function Whereabouts({ folder, branch }: { folder: string; branch: string | null }) {
  const api = useWorkspaceApi();
  const [changed, setChanged] = useState<number>();
  useEffect(() => {
    let current = true;
    void api
      .gitStatus()
      .then((status) => current && setChanged(status.repository ? status.files.length : undefined))
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [api]);
  if (!branch) return <p className="welcome-where">{folder}</p>;
  return (
    <p className="welcome-where">
      {folder}, on <b>{branch}</b>
      {changed === undefined
        ? "."
        : changed === 0
          ? ", with nothing changed since the last commit."
          : `, with ${changed} ${changed === 1 ? "file" : "files"} changed since the last commit.`}
    </p>
  );
}

/** What a new session opens on. The composer sits right under it, and the starters under that. */
export function Welcome({ folder, branch }: { folder: string; branch: string | null }) {
  return (
    <div className="welcome">
      <Veena />
      <h2>What are we working on?</h2>
      <Whereabouts folder={folder} branch={branch} />
    </div>
  );
}

export function Starters({ folder, onPick }: { folder: string; onPick: (text: string) => void }) {
  return (
    <div className="starters" aria-label={`Start a task in ${folder}`}>
      {STARTERS.map((starter) => (
        <button type="button" className="start-row" key={starter.text} onClick={() => onPick(starter.text)}>
          <Icon name={starter.icon} />
          {starter.text}
        </button>
      ))}
    </div>
  );
}

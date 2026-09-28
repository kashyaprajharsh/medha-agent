import { Icon } from "./Icon";
import { medhaIcon } from "./Sidebar";
import { Veena } from "./Veena";

const STARTERS = [
  { icon: "branch", text: "Review my uncommitted changes before I commit" },
  { icon: "tool", text: "Explain how this project is structured" },
  { icon: "term", text: "Find the slowest tests in the workspace" },
] as const;

export function Welcome({
  folder,
  onPick,
}: {
  folder: string;
  onPick: (text: string) => void;
}) {
  return (
    <div className="welcome">
      <img className="welcome-logo" src={medhaIcon} alt="" />
      <h2 className="welcome-name">Medha</h2>
      <Veena />
      <p className="welcome-tag">
        Verification-first and open source. Your machine, your keys.
      </p>
      <div className="starters" aria-label={`Start a task in ${folder}`}>
        {STARTERS.map((starter) => (
          <button
            type="button"
            className="starter"
            key={starter.text}
            onClick={() => onPick(starter.text)}
          >
            <Icon name={starter.icon} />
            {starter.text}
          </button>
        ))}
      </div>
    </div>
  );
}

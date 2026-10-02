import { useEffect, useState } from "react";
import { desktop } from "./api";

const AGAIN = 6 * 60 * 60 * 1000;

export function UpdateReady({ busy }: { busy: boolean }) {
  const [version, setVersion] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [problem, setProblem] = useState("");

  useEffect(() => {
    let live = true;
    // A failed look is not worth a word; the next one tries again.
    const look = () =>
      desktop
        .updateCheck()
        .then((found) => live && found && setVersion(found.version))
        .catch(() => {});
    void look();
    const timer = window.setInterval(look, AGAIN);
    return () => {
      live = false;
      window.clearInterval(timer);
    };
  }, []);

  if (!version) return null;

  const restart = () => {
    setStarting(true);
    setProblem("");
    desktop.updateApply().catch((error) => {
      setStarting(false);
      setProblem(String(error));
    });
  };

  return (
    <div className="side-update" role="status">
      <span className="side-update-text">
        <b>Medha {version} is ready</b>
        <small>
          {problem
            ? "It could not be installed. Try again."
            : busy
              ? "Restart once the chat has finished."
              : "Restart to start using it."}
        </small>
      </span>
      <button
        type="button"
        className="btn-line"
        onClick={restart}
        disabled={busy || starting}
        title={problem || undefined}
      >
        {starting ? "Restarting…" : "Restart"}
      </button>
    </div>
  );
}

import { useEffect, useState } from "react";
import { Spinner } from "./Spinner";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";

type Task = { id: string; command: string; running: boolean };

/** Shell commands Medha left running in the background. Shown only when there
 * are some, and read from the open chat without starting one. */
export function BackgroundTasks({ sessionKey, live, version }: { sessionKey: string | null; live: boolean; version: string }) {
  const api = useWorkspaceApi();
  const [tasks, setTasks] = useState<Task[]>([]);
  const running = tasks.some((task) => task.running);

  useEffect(() => {
    if (!sessionKey || !live) {
      setTasks([]);
      return;
    }
    let active = true;
    const read = () =>
      api
        .liveCall(sessionKey, "tasks.list")
        .then((result) => active && setTasks((result.tasks as Task[]) ?? []))
        .catch(() => active && setTasks([]));
    void read();
    const timer = running ? setInterval(() => void read(), 3000) : undefined;
    return () => {
      active = false;
      if (timer) clearInterval(timer);
    };
  }, [sessionKey, live, version, running]);

  if (!tasks.length) return null;
  return (
    <section className="bg-tasks" aria-label="Background tasks">
      <h3>Background tasks</h3>
      <ul>
        {tasks.map((task) => (
          <li key={task.id}>
            {task.running ? <Spinner /> : <Icon name="check" />}
            <code title={task.command}>{task.command}</code>
            <small>{task.running ? "Running" : "Done"}</small>
          </li>
        ))}
      </ul>
    </section>
  );
}

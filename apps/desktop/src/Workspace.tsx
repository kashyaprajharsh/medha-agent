import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MutableRefObject,
} from "react";
import { App } from "./App";
import {
  createApi,
  desktop,
  type Scope,
  type Workspace,
  type WorkspaceApi,
} from "./api";

const Context = createContext<{
  api: WorkspaceApi;
  workspace: Workspace;
  workspaces: Workspace[];
  active: boolean;
  scope: MutableRefObject<Scope>;
  select: (id: string) => void;
  choose: () => Promise<void>;
  theme: string;
  setTheme: (theme: string) => void;
  textSize: string;
  setTextSize: (size: string) => void;
} | null>(null);
export function useWorkspace() {
  const value = useContext(Context);
  if (!value) throw new Error("Workspace provider missing");
  return value;
}
export function useWorkspaceApi() {
  return useWorkspace().api;
}
function preference(key: string, fallback: string) {
  try {
    return localStorage.getItem(key) || fallback;
  } catch {
    return fallback;
  }
}
export function WorkspaceHost() {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [selected, setSelected] = useState("");
  const [visited, setVisited] = useState<string[]>([]);
  const [error, setError] = useState("");
  const [theme, setTheme] = useState(() => preference("medha-theme", "dark"));
  const [textSize, setTextSize] = useState(() =>
    preference("medha-text-size", "default"),
  );
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.dataset.textSize = textSize;
    try {
      localStorage.setItem("medha-theme", theme);
      localStorage.setItem("medha-text-size", textSize);
    } catch {
      /* private storage */
    }
  }, [theme, textSize]);
  function select(id: string) {
    setSelected(id);
    setVisited((old) => (old.includes(id) ? old : [...old, id]));
  }
  useEffect(() => {
    let active = true;
    void desktop
      .workspaces()
      .then((result) => {
        if (!active) return;
        setWorkspaces(result.workspaces);
        select(result.initial);
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, []);
  async function choose() {
    const workspace = await desktop.chooseWorkspace();
    if (!workspace) return;
    setWorkspaces((old) =>
      old.some((item) => item.id === workspace.id) ? old : [...old, workspace],
    );
    select(workspace.id);
  }
  if (error)
    return (
      <div className="empty">
        <h2>Couldn’t open Medha</h2>
        <p role="alert">{error}</p>
      </div>
    );
  if (!selected)
    return (
      <div className="empty">
        <p>Opening Medha…</p>
      </div>
    );
  return visited.map((id) => (
    <WorkspaceWindow
      key={id}
      workspace={workspaces.find((item) => item.id === id)!}
      workspaces={workspaces}
      active={selected === id}
      select={select}
      choose={choose}
      theme={theme}
      setTheme={setTheme}
      textSize={textSize}
      setTextSize={setTextSize}
    />
  ));
}
function WorkspaceWindow(
  props: Omit<NonNullable<React.ContextType<typeof Context>>, "api" | "scope">,
) {
  const scope = useRef<Scope>({ chatKey: null, sessionId: null });
  const api = useMemo(
    () => createApi(props.workspace.id, () => scope.current),
    [props.workspace.id],
  );
  return (
    <Context.Provider value={{ ...props, api, scope }}>
      <div
        className="workspace-window"
        hidden={!props.active}
        inert={!props.active}
      >
        <App />
      </div>
    </Context.Provider>
  );
}

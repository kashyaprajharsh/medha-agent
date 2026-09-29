import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MutableRefObject,
} from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
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
  mode: string;
  look: string;
  setLook: (look: string) => void;
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
function useSystemDark() {
  const query = "(prefers-color-scheme: dark)";
  const [dark, setDark] = useState(() => matchMedia(query).matches);
  useEffect(() => {
    const media = matchMedia(query);
    const change = () => setDark(media.matches);
    media.addEventListener("change", change);
    return () => media.removeEventListener("change", change);
  }, []);
  return dark;
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
  const [mode, setMode] = useState(() => preference("medha-theme", "system"));
  const [look, setLook] = useState(() => preference("medha-look", "soft"));
  const [textSize, setTextSize] = useState(() =>
    preference("medha-text-size", "default"),
  );
  const systemDark = useSystemDark();
  const theme = mode === "system" ? (systemDark ? "dark" : "light") : mode;
  useEffect(() => {
    const root = document.documentElement;
    root.dataset.platform = /Mac/.test(navigator.userAgent) ? "mac" : "other";
    root.dataset.switching = "";
    root.dataset.theme = theme;
    root.dataset.look = look;
    root.dataset.textSize = textSize;
    void getCurrentWindow()
      .setTheme(mode === "system" ? null : theme === "light" ? "light" : "dark")
      .catch(() => {});
    const settle = requestAnimationFrame(() =>
      requestAnimationFrame(() => delete root.dataset.switching),
    );
    try {
      localStorage.setItem("medha-theme", mode);
      localStorage.setItem("medha-look", look);
      localStorage.setItem("medha-text-size", textSize);
    } catch {
      /* private storage */
    }
    return () => cancelAnimationFrame(settle);
  }, [theme, mode, look, textSize]);
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
      setTheme={setMode}
      mode={mode}
      look={look}
      setLook={setLook}
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

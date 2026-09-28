import { useWorkspaceApi } from "./Workspace";
import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { Icon } from "./Icon";

type Tab = {
  key: string;
  label: string;
  workspace?: string;
  status: "starting" | "running" | "exited" | "error";
  error?: string;
};
let sequence = 0;
const newTab = (): Tab => ({
  key: `terminal-${Date.now()}-${++sequence}`,
  label: "Shell",
  status: "starting",
});

function TerminalPane({
  tab,
  active,
  onUpdate,
}: {
  tab: Tab;
  active: boolean;
  onUpdate: (change: Partial<Tab>) => void;
}) {
  const api = useWorkspaceApi();
  const container = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | undefined>(undefined);
  const update = useRef(onUpdate);
  update.current = onUpdate;

  useEffect(() => {
    if (!container.current) return;
    let disposed = false;
    let ready = false;
    let exited = false;
    let writeQueue = Promise.resolve();
    let fitting = 0;
    const ptyKey = `${tab.key}-${++sequence}`;
    const term = new Terminal({
      fontFamily: 'Menlo, Monaco, "SFMono-Regular", Consolas, monospace',
      fontSize: 14,
      lineHeight: 1.3,
      cursorBlink: true,
      scrollback: 5000,
      disableStdin: true,
      allowProposedApi: false,
    });
    terminal.current = term;
    const colors = () => {
      const css = getComputedStyle(document.documentElement);
      term.options.theme = {
        background: css.getPropertyValue("--canvas").trim(),
        foreground: css.getPropertyValue("--text").trim(),
        cursor: css.getPropertyValue("--gold").trim(),
        selectionBackground: css.getPropertyValue("--line").trim(),
      };
    };
    colors();
    const theme = new MutationObserver(colors);
    theme.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container.current);
    const fitVisible = () => {
      if (!container.current?.clientHeight || !container.current?.clientWidth)
        return;
      fit.fit();
    };
    fitVisible();
    const report = (cause: unknown) => {
      if (!disposed && !exited) update.current({ error: String(cause) });
    };
    const resize = term.onResize(({ cols, rows }) => {
      if (ready && !exited)
        void api.terminalResize(ptyKey, cols, rows).catch(report);
    });
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(fitting);
      fitting = requestAnimationFrame(fitVisible);
    });
    observer.observe(container.current);
    term.attachCustomKeyEventHandler((event) => {
      if (
        (event.ctrlKey && event.code === "Backquote") ||
        (event.metaKey &&
          ["b", "k", "n", "."].includes(event.key.toLowerCase()))
      )
        return false;
      return true;
    });
    const input = term.onData((data) => {
      if (!ready || exited) return;
      // Serialize input and bound each IPC write without splitting Unicode.
      const characters = Array.from(data);
      for (let start = 0; start < characters.length; start += 8192) {
        const chunk = characters.slice(start, start + 8192).join("");
        writeQueue = writeQueue
          .then(() => (disposed ? undefined : api.terminalWrite(ptyKey, chunk)))
          .catch(report);
      }
    });
    // React's development effect replay must not start two shells.
    void Promise.resolve()
      .then(() => {
        if (disposed) return undefined;
        return api.terminalOpen(ptyKey, term.cols, term.rows, (frame) => {
          if (disposed) return;
          if (frame.kind === "output") term.write(new Uint8Array(frame.data));
          else {
            exited = true;
            ready = false;
            term.options.disableStdin = true;
            update.current({ status: "exited", error: undefined });
            term.write(
              `\r\n[Shell exited${frame.code === null ? "" : ` · ${frame.code}`}]\r\n`,
            );
          }
        });
      })
      .then((info) => {
        if (!info) return;
        if (disposed) {
          void api.terminalClose(ptyKey).catch(() => {});
          return;
        }
        ready = !exited;
        term.options.disableStdin = exited;
        update.current({
          label: info.shell,
          workspace: info.workspace,
          status: exited ? "exited" : "running",
        });
        fitVisible();
        if (!exited) {
          void api.terminalResize(ptyKey, term.cols, term.rows).catch(report);
          if (container.current?.clientHeight) term.focus();
        }
      })
      .catch((cause) => {
        if (!disposed)
          update.current({ status: "error", error: String(cause) });
      });
    return () => {
      disposed = true;
      ready = false;
      cancelAnimationFrame(fitting);
      observer.disconnect();
      theme.disconnect();
      input.dispose();
      resize.dispose();
      term.dispose();
      terminal.current = undefined;
      void api.terminalClose(ptyKey).catch(() => {});
    };
  }, [tab.key]);

  useEffect(() => {
    if (active) requestAnimationFrame(() => terminal.current?.focus());
  }, [active]);

  return (
    <div
      className="terminal-pane"
      hidden={!active}
      role="tabpanel"
      id={tab.key}
      aria-label={tab.label}
    >
      <div className="terminal-canvas" ref={container} />
      {tab.error && (
        <p className="terminal-error" role="alert">
          {tab.error}
        </p>
      )}
    </div>
  );
}

export function TerminalDrawer({
  open,
  workspace,
  onHide,
}: {
  open: boolean;
  workspace: string;
  onHide: () => void;
}) {
  const [tabs, setTabs] = useState<Tab[]>(() => [newTab()]);
  const [selected, setSelected] = useState(() => "");
  const [height, setHeight] = useState(280);
  const [maximized, setMaximized] = useState(false);
  const drawer = useRef<HTMLElement>(null);
  const active = selected || tabs[0]?.key;
  const location =
    tabs.find((tab) => tab.key === active)?.workspace || workspace;
  const limitHeight = (value: number) =>
    Math.max(160, Math.min(window.innerHeight * 0.65, value));

  function add() {
    if (tabs.length >= 4) return;
    const tab = newTab();
    setTabs((previous) => [...previous, tab]);
    setSelected(tab.key);
  }
  function close(key: string) {
    const remaining = tabs.filter((tab) => tab.key !== key);
    setTabs(remaining);
    if (key === active) setSelected(remaining.at(-1)?.key ?? "");
  }
  return (
    <section
      ref={drawer}
      className="terminal-drawer"
      hidden={!open}
      aria-label="Your terminal"
      style={{ height: maximized ? "65vh" : height }}
    >
      <div
        className="terminal-grip"
        role="separator"
        aria-orientation="horizontal"
        aria-label="Resize terminal"
        tabIndex={0}
        aria-valuemin={160}
        aria-valuemax={Math.round(window.innerHeight * 0.65)}
        aria-valuenow={Math.round(
          maximized ? window.innerHeight * 0.65 : height,
        )}
        onKeyDown={(event) => {
          if (event.key === "ArrowUp" || event.key === "ArrowDown") {
            event.preventDefault();
            setMaximized(false);
            setHeight((previous) =>
              limitHeight(previous + (event.key === "ArrowUp" ? 24 : -24)),
            );
          }
        }}
        onPointerDown={(event) => {
          const grip = event.currentTarget;
          grip.setPointerCapture(event.pointerId);
          const initial = drawer.current?.clientHeight ?? height;
          const start = event.clientY;
          setMaximized(false);
          grip.onpointermove = (move) =>
            setHeight(limitHeight(initial + start - move.clientY));
          grip.onpointerup = grip.onpointercancel = () => {
            grip.onpointermove = null;
          };
        }}
      />
      <header className="terminal-head">
        <div className="terminal-tabs" role="tablist" aria-label="Terminals">
          {tabs.map((tab) => (
            <div className="terminal-tab" key={tab.key}>
              <button
                type="button"
                role="tab"
                aria-selected={active === tab.key}
                aria-controls={tab.key}
                onClick={() => setSelected(tab.key)}
              >
                <Icon name="term" />
                {tab.label}
                {tab.status === "exited" && <small>exited</small>}
              </button>
              <button
                type="button"
                className="terminal-tab-close"
                aria-label={`Close ${tab.label} terminal`}
                title="Close terminal and stop its shell"
                onClick={() => close(tab.key)}
              >
                <Icon name="x" />
              </button>
            </div>
          ))}
          <button
            type="button"
            className="icon-btn sm"
            onClick={add}
            disabled={tabs.length >= 4}
            title="New terminal"
            aria-label="New terminal"
          >
            <Icon name="plus" />
          </button>
        </div>
        <span className="terminal-location" title={location}>
          {location.split(/[\\/]/).filter(Boolean).at(-1)}
        </span>
        <button
          type="button"
          className="icon-btn sm"
          onClick={() => setMaximized(!maximized)}
          aria-label={maximized ? "Restore terminal height" : "Expand terminal"}
          title={maximized ? "Restore height" : "Expand terminal"}
        >
          <Icon name="maximize" />
        </button>
        <button
          type="button"
          className="icon-btn sm"
          onClick={onHide}
          aria-label="Hide terminal"
          title="Hide terminal · ⌃`"
        >
          <Icon name="down" />
        </button>
      </header>
      <div className="terminal-panes">
        {tabs.map((tab) => (
          <TerminalPane
            key={tab.key}
            tab={tab}
            active={open && active === tab.key}
            onUpdate={(change) =>
              setTabs((previous) =>
                previous.map((item) =>
                  item.key === tab.key ? { ...item, ...change } : item,
                ),
              )
            }
          />
        ))}
        {tabs.length === 0 && (
          <div className="terminal-empty">
            <p>No terminals open</p>
            <button type="button" className="btn-line" onClick={add}>
              New terminal
            </button>
          </div>
        )}
      </div>
    </section>
  );
}

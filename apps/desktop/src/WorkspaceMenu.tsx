import { useEffect, useRef, useState } from "react";
import { useWorkspace } from "./Workspace";
import { Icon } from "./Icon";
export function WorkspaceMenu({
  folder,
  workspace,
}: {
  folder: string;
  workspace: string;
}) {
  const context = useWorkspace();
  const [open, setOpen] = useState(false);
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const root = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const outside = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);
  async function act(action: "choose" | "copy" | "reveal") {
    setNotice("");
    setBusy(true);
    try {
      if (action === "choose") {
        await context.choose();
        setOpen(false);
      } else if (action === "copy") {
        await navigator.clipboard.writeText(workspace);
        setNotice("Path copied");
      } else {
        await context.api.revealWorkspace();
        setOpen(false);
      }
    } catch (cause) {
      setNotice(String(cause));
    } finally {
      setBusy(false);
    }
  }
  return (
    <span className="workspace-control" ref={root}>
      <button
        type="button"
        className="workspace-trigger"
        aria-expanded={open}
        title={workspace}
        onClick={() => setOpen(!open)}
      >
        <Icon name={context.workspace.personal ? "chat" : "folder"} />
        {folder}
        <Icon name="down" />
      </button>
      {open && (
        <span className="workspace-popover">
          <b>Workspace</b>
          {context.workspaces.map((item) => (
            <button
              type="button"
              key={item.id}
              aria-current={
                item.id === context.workspace.id ? "true" : undefined
              }
              onClick={() => {
                context.select(item.id);
                setOpen(false);
              }}
            >
              <Icon name={item.personal ? "chat" : "folder"} />
              <span>{item.name}</span>
              {item.id === context.workspace.id && <Icon name="check" />}
            </button>
          ))}
          <button
            type="button"
            disabled={busy}
            onClick={() => void act("choose")}
          >
            <Icon name="plus" />
            Open project folder…
          </button>
          <span className="workspace-path">{workspace}</span>
          <button
            type="button"
            disabled={busy}
            onClick={() => void act("reveal")}
          >
            <Icon name="folder" />
            Show in file manager
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void act("copy")}
          >
            <Icon name="copy" />
            Copy path
          </button>
          {notice && (
            <span role="status" className="quiet">
              {notice}
            </span>
          )}
        </span>
      )}
    </span>
  );
}

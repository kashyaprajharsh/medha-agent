import { Fragment, useState, type DragEvent } from "react";
import { desktop, type ExtensionSourceKind } from "./api";
import { McpForm } from "./McpServers";
import { Sheet } from "./Sheet";
import { useWorkspaceApi } from "./Workspace";

/** What `extensions.preview` says a source is; nothing has been added yet. */
export type Preview =
  | { kind: "skill"; name?: string; description?: string; files?: string[]; findings?: string[]; refused?: string }
  | {
      kind: "plugin";
      name: string;
      description?: string;
      access: { network_hosts: string[]; read_paths: string[]; write_paths: string[]; secrets: string[] };
      adds: { id: string; kind: "skill" | "action" | "mcp" | "hook" }[];
      findings: string[];
      refused?: string | null;
    }
  | { kind: "connector"; name: string; url: string };

type Props = {
  disabled: boolean;
  onClose: () => void;
  /** Start a skill from nothing, in the editor. */
  onWrite: () => void;
  /** Start a new chat that drafts a skill with the built-in skill creator. */
  onCreate: (prompt: string) => void;
  onAdd: (method: string, params: Record<string, unknown>) => Promise<void>;
  onConnector: (args: string[]) => Promise<void>;
};

const KIND = { skill: "Skill", plugin: "Plugin", connector: "Connector" } as const;
const PARTS = [
  ["skill", "Skills"],
  ["action", "Commands"],
  ["mcp", "Servers"],
  ["hook", "On its own"],
] as const;

/** One way in for a skill, a plugin or a connector: given anything, it says what it is before it is added. */
export function Intake({ disabled, onClose, onWrite, onCreate, onAdd, onConnector }: Props) {
  const api = useWorkspaceApi();
  const [typed, setTyped] = useState("");
  const [source, setSource] = useState("");
  const [shown, setShown] = useState("");
  const [preview, setPreview] = useState<Preview>();
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState("");
  const [scope, setScope] = useState<"user" | "project">("user");
  const [over, setOver] = useState(false);

  async function look(path: string, label = path) {
    if (!path.trim()) return;
    setChecking(true);
    setError("");
    setPreview(undefined);
    try {
      const seen = (await api.extensions("extensions.preview", { source: path.trim() })) as Preview;
      setSource(path.trim());
      setShown(label);
      setPreview(seen);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setChecking(false);
    }
  }
  async function pick(kind: ExtensionSourceKind) {
    setError("");
    try {
      const path = await desktop.chooseExtensionSource(kind);
      if (path) await look(path, path.split(/[\\/]/).pop() || path);
    } catch (cause) {
      setError(String(cause));
    }
  }
  async function dropped(event: DragEvent) {
    event.preventDefault();
    setOver(false);
    const file = event.dataTransfer.files[0];
    if (!file) return;
    setError("");
    try {
      await look(await desktop.uploadExtension(file), file.name);
    } catch (cause) {
      setError(String(cause));
    }
  }
  const change = () => {
    setPreview(undefined);
    setSource("");
    setError("");
  };
  const add = async (method: string, params: Record<string, unknown>) => {
    await onAdd(method, params);
    onClose();
  };
  const busy = disabled || checking;

  return (
    <Sheet label="Add to Medha" onClose={onClose} onBack={preview ? change : undefined}>
      <div className="sheet-title">
        <h2>Add to Medha</h2>
      </div>
      <p className="sheet-lead">
        A skill, a plugin or a connector. Medha works out which, and shows you before anything is added.
      </p>
      {preview ? (
        <div className="intake-drop given">
          <div className="intake-given">
            <span className="intake-kind">{KIND[preview.kind]}</span>
            <span className="intake-source">{shown}</span>
            <button className="btn-line intake-change" onClick={change}>
              Change
            </button>
          </div>
        </div>
      ) : (
        <div
          className={`intake-drop${over ? " over" : ""}`}
          aria-busy={checking}
          onDragOver={(event) => {
            event.preventDefault();
            setOver(true);
          }}
          onDragLeave={() => setOver(false)}
          onDrop={(event) => void dropped(event)}
        >
          <p className="intake-ask">{checking ? "Looking at it" : "Drop a file here, or paste a link or path"}</p>
          <form
            className="intake-paste-row"
            onSubmit={(event) => {
              event.preventDefault();
              void look(typed);
            }}
          >
            <input
              className="intake-paste"
              aria-label="Link or path"
              autoFocus
              disabled={busy}
              value={typed}
              onChange={(event) => setTyped(event.target.value)}
              placeholder="A GitHub link, owner/repo, name@marketplace, or https://mcp.example.com/mcp"
            />
            <button className="btn-line" disabled={busy || !typed.trim()}>
              Look
            </button>
          </form>
          <div className="intake-pick">
            <button type="button" className="btn-line" disabled={busy} onClick={() => void pick("file")}>
              Choose file
            </button>
            <button type="button" className="btn-line" disabled={busy} onClick={() => void pick("folder")}>
              Choose folder
            </button>
          </div>
          <p className="intake-kinds">
            Takes a .zip, .skill, .plugin, SKILL.md, a folder, a GitHub link, a marketplace listing or an MCP server URL.
          </p>
        </div>
      )}
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      {!preview && (
        <>
          <h3 className="sheet-h intake-scratch">Or start from nothing</h3>
          <div className="intake-starts">
            <button className="intake-start" disabled={disabled} onClick={onWrite}>
              <b>Write a skill</b>
              <span>An editor that checks it as you type and shows how Medha will list it.</span>
            </button>
            <button
              className="intake-start"
              onClick={() => {
                onClose();
                onCreate("Use the skill-creator skill to help me write a skill that ");
              }}
            >
              <b>Create with Medha</b>
              <span>A new chat that drafts it with you. You read it before it is saved, or finish it in the editor.</span>
            </button>
          </div>
        </>
      )}
      {preview?.kind === "skill" && (
        <SkillPreview
          preview={preview}
          scope={scope}
          onScope={setScope}
          disabled={busy}
          onAdd={() => void add("extensions.skill.install", { source, scope })}
          onCancel={onClose}
        />
      )}
      {preview?.kind === "plugin" && (
        <PluginPreview
          preview={preview}
          disabled={busy}
          onAdd={() => void add("extensions.install", { source })}
          onCancel={onClose}
        />
      )}
      {preview?.kind === "connector" && (
        <>
          <h3 className="sheet-h">Connect it</h3>
          <p className="intake-note">
            Only connect servers you trust: Medha cannot see what a server does with what it is sent.
          </p>
          <McpForm
            busy={busy}
            initial={{ id: preview.name, transport: "remote", url: preview.url }}
            submitLabel="Add connector"
            onSave={async (args) => {
              await onConnector(args);
              onClose();
            }}
          />
        </>
      )}
    </Sheet>
  );
}

function Findings({ findings, refused }: { findings: string[]; refused?: string | null }) {
  if (refused)
    return (
      <>
        <dt>Scan</dt>
        <dd className="warn">
          It cannot be added: the scan found what could harm on install.
          {refused
            .split("\n")
            .slice(1)
            .map((line) => (
              <span className="sheet-finding" key={line}>
                {line.trim()}
              </span>
            ))}
        </dd>
      </>
    );
  return (
    <>
      <dt>Scan</dt>
      {findings.length ? (
        <dd className="warn">
          {findings.length === 1 ? "1 thing" : `${findings.length} things`} to read before adding:
          {findings.map((finding) => (
            <span className="sheet-finding" key={finding}>
              {finding}
            </span>
          ))}
        </dd>
      ) : (
        <dd>Nothing found</dd>
      )}
    </>
  );
}

function SkillPreview({
  preview,
  scope,
  onScope,
  disabled,
  onAdd,
  onCancel,
}: {
  preview: Extract<Preview, { kind: "skill" }>;
  scope: "user" | "project";
  onScope: (scope: "user" | "project") => void;
  disabled: boolean;
  onAdd: () => void;
  onCancel: () => void;
}) {
  return (
    <>
      {preview.name && (
        <>
          <h3 className="sheet-h">How Medha will see it</h3>
          <p className="intake-listing">
            <b>{preview.name}</b>
            <span>{preview.description}</span>
          </p>
        </>
      )}
      <dl className="sheet-facts">
        {preview.files && (
          <>
            <dt>Files</dt>
            <dd className="mono intake-filelist">
              {preview.files.map((file) => (
                <span key={file}>{file}</span>
              ))}
            </dd>
          </>
        )}
        <Findings findings={preview.findings ?? []} refused={preview.refused} />
        {!preview.refused && (
          <>
            <dt>For</dt>
            <dd>
              <label className="intake-choice">
                <input type="radio" name="intake-for" checked={scope === "user"} onChange={() => onScope("user")} /> Me,
                every project
              </label>
              <label className="intake-choice">
                <input
                  type="radio"
                  name="intake-for"
                  checked={scope === "project"}
                  onChange={() => onScope("project")}
                />{" "}
                This project only
              </label>
            </dd>
          </>
        )}
      </dl>
      {preview.findings?.length ? (
        <p className="intake-note">It is added turned off, so you can read these first and then turn it on.</p>
      ) : null}
      <div className="sheet-actions">
        {!preview.refused && (
          <button className="btn-gold" disabled={disabled} onClick={onAdd}>
            Add skill
          </button>
        )}
        <button className="btn-line" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </>
  );
}

function PluginPreview({
  preview,
  disabled,
  onAdd,
  onCancel,
}: {
  preview: Extract<Preview, { kind: "plugin" }>;
  disabled: boolean;
  onAdd: () => void;
  onCancel: () => void;
}) {
  const { network_hosts, read_paths, write_paths, secrets } = preview.access;
  const list = (items: string[], none: string) => (items.length ? items.join(", ") : none);
  return (
    <>
      <h3 className="sheet-h">What it adds</h3>
      <p className="intake-listing">
        <b>{preview.name}</b>
        {preview.description && <span>{preview.description}</span>}
      </p>
      <dl className="sheet-facts">
        {PARTS.map(([kind, title]) => {
          const parts = preview.adds.filter((part) => part.kind === kind).map((part) => part.id);
          return parts.length ? (
            <Fragment key={kind}>
              <dt>{title}</dt>
              <dd className={kind === "hook" ? "warn" : undefined}>
                {kind === "hook"
                  ? `${parts.length === 1 ? "1 script that runs" : `${parts.length} scripts that run`} without being asked: ${parts.join(", ")}`
                  : parts.join(", ")}
              </dd>
            </Fragment>
          ) : null;
        })}
        {!preview.adds.length && (
          <>
            <dt>Parts</dt>
            <dd>Nothing yet</dd>
          </>
        )}
      </dl>
      <h3 className="sheet-h">What it can touch</h3>
      <dl className="sheet-facts">
        <dt>Internet</dt>
        <dd>{network_hosts.length ? `Only ${network_hosts.join(", ")}` : "Nothing"}</dd>
        <dt>Reads</dt>
        <dd>{list(read_paths, "Nothing beyond its own files")}</dd>
        <dt>Changes</dt>
        <dd>{list(write_paths, "Nothing")}</dd>
        {secrets.length > 0 && (
          <>
            <dt>Keys</dt>
            <dd>{secrets.join(", ")}</dd>
          </>
        )}
        <Findings findings={preview.findings} refused={preview.refused} />
      </dl>
      {!preview.refused && (
        <p className="intake-note">
          It stays off until you turn it on, and you choose where it is on then, so nothing runs yet.
        </p>
      )}
      <div className="sheet-actions">
        {!preview.refused && (
          <button className="btn-gold" disabled={disabled} onClick={onAdd}>
            Add plugin
          </button>
        )}
        <button className="btn-line" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </>
  );
}

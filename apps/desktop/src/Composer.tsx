import { Select } from "./Select";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  desktop,
  type Attachment,
  SessionChange,
  SessionSettings,
} from "./api";
import { Icon, type IconName } from "./Icon";

export type Control = "model" | "reasoning" | "mode";
type Props = {
  value: string;
  resetRevision?: number;
  onChange: (value: string) => void;
  onSend: () => void;
  onStop: () => void;
  running: boolean;
  disabled: boolean;
  placeholder: string;
  model?: string;
  contextPercent?: number;
  settings?: SessionSettings;
  settingsLocked: boolean;
  control: Control | null;
  onControl: (control: Control | null) => void;
  onConfigure: (change: SessionChange) => Promise<void>;
  onLoadSettings: () => Promise<void>;
  showReasoning: boolean;
  onShowReasoning: (show: boolean) => void;
  attachments: Attachment[];
  onAttachments: (attachments: Attachment[]) => void;
  onSurface: (tab: "agents" | "changes" | "files" | "context") => void;
  onNew: () => void;
  onRewind: () => void;
  onSettings: () => void;
  onExtensions: () => void;
};

const MODES = [
  {
    id: "plan",
    title: "Plan",
    description: "Investigate and plan. Read-only.",
  },
  {
    id: "careful",
    title: "Careful",
    description: "Ask before edits and shell commands.",
  },
  {
    id: "normal",
    title: "Normal",
    description: "Apply edits. Ask before shell commands.",
  },
  {
    id: "yolo",
    title: "Yolo",
    description: "Run edits and shell. Dangerous actions still ask.",
  },
];
const COMMANDS: { id: string; icon: IconName; description: string }[] = [
  { id: "model", icon: "cpu", description: "Choose a saved model" },
  { id: "reasoning", icon: "brain", description: "Thinking and effort" },
  { id: "mode", icon: "shield", description: "How Medha handles changes" },
  { id: "agents", icon: "fork", description: "Inspect and direct agents" },
  {
    id: "changes",
    icon: "branch",
    description: "Review file changes and agent patches",
  },
  { id: "context", icon: "list", description: "Context and usage" },
  { id: "rewind", icon: "rewind", description: "Rewind conversation or files" },
  {
    id: "clear",
    icon: "chat",
    description: "Start fresh, keep chat in history",
  },
  {
    id: "settings",
    icon: "settings",
    description: "Models, search and instructions",
  },
  { id: "plugins", icon: "blocks", description: "Skills, plugins and MCP" },
  { id: "files", icon: "folder", description: "Browse workspace files" },
  { id: "new", icon: "plus", description: "Start a new session" },
];

export function Composer(props: Props) {
  // Keystrokes belong to the composer. The app keeps a draft reference, so
  // typing does not rerender the sidebar, transcript and workspace surfaces.
  const [value, setValue] = useState(props.value);
  const reset = props.resetRevision ?? props.value;
  useLayoutEffect(() => {
    setValue(props.value);
    // Stream renders may carry an older draft. Only explicit app resets should
    // replace locally typed text; standalone controlled callers use value.
  }, [reset]);
  function change(value: string) {
    setValue(value);
    props.onChange(value);
  }
  const input = useRef<HTMLTextAreaElement>(null);
  const chooser = useRef<HTMLInputElement>(null);
  const controls = useRef<HTMLDivElement>(null);
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [dragging, setDragging] = useState(false);
  const [commandIndex, setCommandIndex] = useState(0);
  const [commandHidden, setCommandHidden] = useState(false);
  const slash =
    !commandHidden && /^\/[a-z]*$/i.test(value)
      ? value.slice(1).toLowerCase()
      : null;
  const commands =
    slash === null
      ? []
      : COMMANDS.filter((command) => command.id.startsWith(slash));

  useEffect(() => {
    const box = input.current;
    if (!box) return;
    box.style.height = "auto";
    box.style.height = `${Math.min(box.scrollHeight, 200)}px`;
  }, [value]);
  useEffect(() => {
    setCommandIndex(0);
  }, [value]);
  useEffect(() => {
    if (!props.control) return;
    setQuery("");
    setError(undefined);
    setLoading(true);
    let active = true;
    void props
      .onLoadSettings()
      .catch((cause) => active && setError(String(cause)))
      .finally(() => active && setLoading(false));
    const outside = (event: PointerEvent) => {
      if (
        !(event.target as Element).closest(".select-menu") &&
        !controls.current?.contains(event.target as Node)
      )
        props.onControl(null);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        props.onControl(null);
        input.current?.focus();
      }
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      active = false;
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [props.control]);

  async function configure(change: SessionChange) {
    setBusy(true);
    setError(undefined);
    try {
      await props.onConfigure(change);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }
  async function attach(files: File[]) {
    if (props.running) {
      setError("Attach images after this turn finishes.");
      return;
    }
    if (props.attachments.length + files.length > 4) {
      setError("Attach up to four images at a time.");
      return;
    }
    setBusy(true);
    setError(undefined);
    try {
      const added = await Promise.all(
        files.map(async (file): Promise<Attachment> => {
          if (file.size > 64 * 1024 * 1024)
            throw new Error(`${file.name} exceeds the 64 MB source limit.`);
          const admitted = await desktop.admitImage(
            new Uint8Array(await file.arrayBuffer()),
          );
          const preview = `data:${admitted.mime};base64,${admitted.data}`;
          return {
            id: crypto.randomUUID(),
            name: file.name,
            mime: admitted.mime,
            data: admitted.data,
            note: admitted.note,
            preview,
          };
        }),
      );
      props.onAttachments([...props.attachments, ...added]);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
      if (chooser.current) chooser.current.value = "";
    }
  }
  function command(id: string) {
    change("");
    if (id === "model" || id === "reasoning" || id === "mode")
      props.onControl(id);
    else if (id === "new") props.onNew();
    else if (id === "clear") {
      if (!props.settingsLocked) props.onNew();
      else setError("Finish active work before starting fresh.");
    } else if (id === "rewind") {
      if (!props.settingsLocked) props.onRewind();
      else setError("Finish active work before rewinding.");
    } else if (id === "settings") props.onSettings();
    else if (id === "plugins") props.onExtensions();
    else props.onSurface(id as "agents" | "changes" | "files" | "context");
  }

  const canSend =
    !props.disabled &&
    !busy &&
    (value.trim().length > 0 || props.attachments.length > 0);
  const ring = Math.min(100, Math.max(0, props.contextPercent ?? 0));
  const locked = busy || loading || props.settingsLocked;
  const mode = MODES.find((mode) => mode.id === props.settings?.mode);
  const profiles =
    props.settings?.profiles.filter((profile) =>
      `${profile.name} ${profile.model}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    ) ?? [];
  const support = props.settings?.reasoning_support;

  return (
    <div className="composer-wrap">
      <form
        className={`composer ${dragging ? "dragging" : ""}`}
        onSubmit={(event) => {
          event.preventDefault();
          if (canSend) props.onSend();
        }}
        onDragOver={(event) => {
          event.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(event) => {
          event.preventDefault();
          setDragging(false);
          if (!busy) void attach([...event.dataTransfer.files]);
        }}
      >
        {props.attachments.length > 0 && (
          <div className="attachments">
            {props.attachments.map((attachment) => (
              <div className="attachment" key={attachment.id}>
                <img src={attachment.preview} alt="" />
                <span title={attachment.note || attachment.name}>
                  {attachment.name}
                  {attachment.note && (
                    <small className="attachment-note">{attachment.note}</small>
                  )}
                </span>
                <button
                  type="button"
                  onClick={() =>
                    props.onAttachments(
                      props.attachments.filter(
                        (item) => item.id !== attachment.id,
                      ),
                    )
                  }
                  aria-label={`Remove ${attachment.name}`}
                >
                  <Icon name="x" />
                </button>
              </div>
            ))}
          </div>
        )}
        {dragging && (
          <div className="drop-hint">
            <Icon name="clip" />
            Drop images here
          </div>
        )}
        <textarea
          ref={input}
          rows={1}
          value={value}
          disabled={props.disabled}
          aria-label="Message Medha"
          placeholder={
            props.running
              ? "Give Medha another instruction…"
              : props.placeholder
          }
          onChange={(event) => {
            change(event.target.value);
            setCommandHidden(false);
          }}
          onPaste={(event) => {
            const files = [...event.clipboardData.files];
            if (files.length && !busy) {
              event.preventDefault();
              void attach(files);
            }
          }}
          onKeyDown={(event) => {
            if (
              commands.length &&
              ["ArrowDown", "ArrowUp"].includes(event.key)
            ) {
              event.preventDefault();
              setCommandIndex(
                (index) =>
                  (index +
                    (event.key === "ArrowDown" ? 1 : commands.length - 1)) %
                  commands.length,
              );
              return;
            }
            if (event.key === "Escape") {
              setCommandHidden(true);
              return;
            }
            if (
              event.key === "Enter" &&
              !event.shiftKey &&
              !event.nativeEvent.isComposing
            ) {
              event.preventDefault();
              if (commands.length)
                command(commands[commandIndex]?.id ?? commands[0].id);
              else if (canSend) props.onSend();
            }
          }}
        />
        {commands.length > 0 && (
          <div className="slash-menu" role="listbox" aria-label="Chat commands">
            {commands.map((item, index) => (
              <button
                type="button"
                key={item.id}
                role="option"
                aria-selected={commandIndex === index}
                onMouseMove={() => setCommandIndex(index)}
                onClick={() => command(item.id)}
              >
                <Icon name={item.icon} />
                <b>/{item.id}</b>
                <span>{item.description}</span>
              </button>
            ))}
          </div>
        )}
        <div className="composer-row" ref={controls}>
          <input
            ref={chooser}
            type="file"
            accept="image/*"
            multiple
            hidden
            onChange={(event) => void attach([...(event.target.files ?? [])])}
          />
          <button
            type="button"
            className="icon-btn"
            onClick={() => chooser.current?.click()}
            disabled={busy || props.running}
            title="Attach images · paste or drag images here"
            aria-label="Attach images"
          >
            <Icon name="clip" />
          </button>
          {(["model", "reasoning", "mode"] as Control[]).map((control) => (
            <button
              type="button"
              key={control}
              className={`chip ${props.control === control ? "active" : ""}`}
              aria-haspopup="dialog"
              aria-expanded={props.control === control}
              onClick={() =>
                props.onControl(props.control === control ? null : control)
              }
            >
              <Icon
                name={
                  control === "model"
                    ? "cpu"
                    : control === "reasoning"
                      ? "brain"
                      : "shield"
                }
              />
              <span className="chip-label">
                {control === "model"
                  ? props.settings?.profile || props.model || "Model"
                  : control === "reasoning"
                    ? props.settings?.reasoning === "off"
                      ? "Thinking off"
                      : props.settings?.effort &&
                          props.settings.effort !== "auto"
                        ? `Thinking · ${props.settings.effort}`
                        : "Thinking"
                    : (mode?.title ?? "Mode")}
              </span>
              <Icon name="down" className="icon chip-caret" />
            </button>
          ))}
          {props.control && (
            <div
              className={`control-popover ${props.control}`}
              role="dialog"
              aria-label={`${props.control} settings`}
            >
              <div className="popover-head">
                <b>
                  {props.control === "model"
                    ? "Choose a model"
                    : props.control === "reasoning"
                      ? "Thinking"
                      : "Execution mode"}
                </b>
                <button
                  type="button"
                  className="icon-btn sm"
                  onClick={() => props.onControl(null)}
                  aria-label="Close settings"
                >
                  <Icon name="x" />
                </button>
              </div>
              {props.control === "model" && (
                <>
                  <div className="model-search">
                    <Icon name="search" />
                    <input
                      autoFocus
                      value={query}
                      onChange={(event) => setQuery(event.target.value)}
                      placeholder="Search saved models"
                      aria-label="Search models"
                    />
                  </div>
                  <div className="model-options">
                    {profiles.map((profile) => (
                      <button
                        type="button"
                        className="setting-option"
                        key={profile.name}
                        disabled={locked}
                        aria-pressed={props.settings?.profile === profile.name}
                        onClick={() =>
                          void configure({ profile: profile.name })
                        }
                      >
                        <Icon name="cpu" />
                        <span>
                          <b>{profile.name}</b>
                          <small>
                            {profile.model}
                            {profile.default ? " · default" : ""}
                          </small>
                        </span>
                        {props.settings?.profile === profile.name && (
                          <Icon name="check" />
                        )}
                      </button>
                    ))}
                    {!profiles.length && (
                      <p className="popover-note">
                        {query
                          ? "No saved models match this search."
                          : "No saved model profiles yet. Your configured connection is used for this session."}
                      </p>
                    )}
                  </div>
                  <button
                    type="button"
                    className="manage-models"
                    onClick={() => {
                      props.onControl(null);
                      props.onSettings();
                    }}
                  >
                    <Icon name="plus" />
                    Add or manage models
                  </button>
                </>
              )}
              {props.control === "mode" &&
                MODES.map((mode) => (
                  <button
                    type="button"
                    className="setting-option"
                    key={mode.id}
                    disabled={locked}
                    aria-pressed={props.settings?.mode === mode.id}
                    onClick={() => void configure({ mode: mode.id })}
                  >
                    <span className="option-radio" />
                    <span>
                      <b>{mode.title}</b>
                      <small>{mode.description}</small>
                    </span>
                  </button>
                ))}
              {props.control === "reasoning" && (
                <>
                  <div className="setting-section">
                    <label>Model reasoning</label>
                    <div className="segmented">
                      {["auto", "on", "off"].map((value) => (
                        <button
                          type="button"
                          disabled={
                            locked ||
                            (support === "unsupported" && value !== "auto")
                          }
                          key={value}
                          aria-pressed={props.settings?.reasoning === value}
                          onClick={() => void configure({ reasoning: value })}
                        >
                          {value === "auto"
                            ? "Auto"
                            : value === "on"
                              ? "On"
                              : "Off"}
                        </button>
                      ))}
                    </div>
                  </div>
                  {support !== "unsupported" && (
                    <div className="setting-section">
                      <label htmlFor="effort">Effort</label>
                      <Select
                        id="effort"
                        disabled={locked}
                        value={props.settings?.effort ?? "auto"}
                        onChange={(event) =>
                          void configure({ effort: event.target.value })
                        }
                      >
                        <option value="auto">Model default</option>
                        {props.settings?.efforts.map((effort) => (
                          <option key={effort} value={effort}>
                            {effort[0].toUpperCase() + effort.slice(1)}
                          </option>
                        ))}
                      </Select>
                    </div>
                  )}
                  <div className="setting-section toggles">
                    <label>
                      Expand thinking in chat
                      <button
                        type="button"
                        role="switch"
                        aria-checked={props.showReasoning}
                        className="switch"
                        onClick={() =>
                          props.onShowReasoning(!props.showReasoning)
                        }
                      />
                    </label>
                    <label>
                      Stream responses
                      <button
                        type="button"
                        role="switch"
                        aria-checked={props.settings?.streaming ?? true}
                        className="switch"
                        disabled={locked}
                        onClick={() =>
                          void configure({
                            streaming: !props.settings?.streaming,
                          })
                        }
                      />
                    </label>
                  </div>
                  <p className="popover-note">
                    {support === "unsupported"
                      ? "This model does not accept reasoning controls."
                      : support === "unverified"
                        ? "Reasoning support is unverified for this profile. The provider validates each choice."
                        : "Effort affects the model. Expanding thinking only changes the display."}
                  </p>
                </>
              )}
              {loading && (
                <p className="popover-note" role="status">
                  Reading session settings…
                </p>
              )}
              {props.settingsLocked && (
                <p className="popover-note">
                  Finish or stop active work to change session settings.
                </p>
              )}
              {error && (
                <p className="popover-error" role="alert">
                  {error}
                </p>
              )}
              <div className="popover-foot">Applies to this session</div>
            </div>
          )}
          <span className="grow" />
          {props.contextPercent !== undefined && (
            <button
              type="button"
              className="ctx"
              onClick={() => props.onSurface("context")}
              title="Context and usage"
              aria-label={`Context ${ring}% used. Open details.`}
            >
              <svg viewBox="0 0 20 20" aria-hidden="true">
                <circle cx="10" cy="10" r="7.5" className="ctx-track" />
                <circle
                  cx="10"
                  cy="10"
                  r="7.5"
                  className="ctx-fill"
                  strokeDasharray={`${(ring / 100) * 47.1} 47.1`}
                />
              </svg>
              {ring}%
            </button>
          )}
          {props.running && (
            <button
              type="button"
              className="stop"
              onClick={props.onStop}
              aria-label="Stop this turn"
              title="Stop this turn"
            >
              <span />
            </button>
          )}
          {(!props.running || canSend) && (
            <button
              type="submit"
              className="send"
              disabled={!canSend}
              aria-label={props.running ? "Send instruction" : "Send message"}
            >
              <Icon name="up" />
            </button>
          )}
        </div>
      </form>
      {error && !props.control && (
        <div className="composer-error" role="alert">
          {error}
          <button
            type="button"
            onClick={() => setError(undefined)}
            aria-label="Dismiss error"
          >
            <Icon name="x" />
          </button>
        </div>
      )}
      <div className={`composer-hint ${props.running ? "running" : ""}`}>
        <span>
          {props.running
            ? "Your instruction arrives at Medha’s next step."
            : "Type / for commands"}
        </span>
        <span>↵ send · ⇧↵ new line</span>
      </div>
    </div>
  );
}

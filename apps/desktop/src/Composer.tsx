import { Select } from "./Select";
import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import {
  desktop,
  type Attachment,
  SessionChange,
  SessionSettings,
} from "./api";
import { Icon, type IconName } from "./Icon";
import { PlanLine } from "./ChatGpt";
import { MessageTokens, type MessageToken, type TokenLayout } from "./MessageTokens";

export type Control = "model" | "reasoning" | "mode";
// The window's own bar above the page, the gap a menu keeps from an edge, and the least a menu is given.
const HEAD = 56;
const MARGIN = 12;
const SMALLEST = 180;

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
  /** Set while no model is saved: the model chip opens setup instead of a list. */
  onConnect?: () => void;
  /** Enabled skills the `/` menu offers alongside its commands. */
  skills?: { name: string; description: string }[];
  /** The output open beside the chat that the next message is about, by its label. */
  about?: string;
  /** The person removed that token: this message is not about the open output. */
  onAbout?: () => void;
  /** The skill riding along with the next message, if one was chosen. */
  skill?: string;
  onSkill?: (name: string | null) => void | Promise<void>;
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
  const latest = useRef(props);
  latest.current = props;
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
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
  const box = useRef<HTMLFormElement>(null);
  // Where a menu has room: the composer sits at the foot of a chat and mid-page on a new session.
  const [room, setRoom] = useState<{ down: boolean; most: number }>();
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [dragging, setDragging] = useState(false);
  const [commandIndex, setCommandIndex] = useState(0);
  const [commandHidden, setCommandHidden] = useState(false);
  const [armed, setArmed] = useState<string | null>(null);
  const [leaving, setLeaving] = useState<string | null>(null);
  const [layout, setLayout] = useState<TokenLayout>({ indent: 0, lift: 0 });
  const tokens: MessageToken[] = [
    ...(props.about ? [{ id: "about", kind: "output" as const, label: props.about, title: `Your message is about ${props.about}, open beside the chat. Remove this to ask for something new.` }] : []),
    ...(props.skill ? [{ id: "skill", kind: "skill" as const, label: props.skill }] : []),
    ...props.attachments.map((attachment) => ({
      id: attachment.id,
      kind: "data" in attachment ? "image" as const : attachment.kind,
      label: attachment.name,
      preview: "data" in attachment ? attachment.preview : undefined,
      title: "data" in attachment ? attachment.note || attachment.name : attachment.path,
    })),
  ];
  const armedToken = tokens.find((token) => token.id === armed);
  function removeToken(id: string) {
    setArmed(null);
    setLeaving(id);
    const drop = () => {
      if (!mounted.current) return;
      setLeaving(null);
      if (id === "about") latest.current.onAbout?.();
      else if (id === "skill") latest.current.onSkill?.(null);
      else latest.current.onAttachments(latest.current.attachments.filter((item) => item.id !== id));
    };
    // Exits are faster than entries; without motion the token just goes.
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) drop();
    else window.setTimeout(drop, 120);
    input.current?.focus();
  }
  const slash =
    !commandHidden && /^\/[a-z0-9-]*$/i.test(value)
      ? value.slice(1).toLowerCase()
      : null;
  const commands: { id: string; icon: IconName; description: string; skill?: boolean }[] =
    slash === null
      ? []
      : [
          ...COMMANDS.filter((command) => command.id.startsWith(slash)),
          ...(props.skills ?? [])
            .filter((skill) => skill.name.toLowerCase().includes(slash))
            .slice(0, 8)
            .map((skill) => ({ id: skill.name, icon: "file" as IconName, description: skill.description, skill: true })),
        ];

  function fit() {
    const box = input.current;
    if (!box) return;
    box.style.height = "auto";
    box.style.height = `${Math.min(box.scrollHeight, 200)}px`;
  }
  useEffect(fit, [value, layout]);
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
    if (busy || !files.length) return;
    if (props.running) {
      setError("Attach images after this turn finishes.");
      return;
    }
    if (files.some((file) => file.type && !file.type.startsWith("image/"))) {
      setError("For documents or folders, use Attach → Files or Folder.");
      return;
    }
    if (props.attachments.filter((item) => "data" in item).length + files.length > 4) {
      setError("Attach up to four images at a time.");
      return;
    }
    if (props.attachments.length + files.length > 16) {
      setError("Attach up to sixteen items at a time.");
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
      if (mounted.current) addAttachments(added);
    } catch (cause) {
      if (mounted.current) setError(String(cause));
    } finally {
      if (mounted.current) {
        setBusy(false);
        input.current?.focus();
      }
      if (chooser.current) chooser.current.value = "";
    }
  }
  const [attachmentMenu, setAttachmentMenu] = useState(false);
  const attachmentButton = useRef<HTMLButtonElement>(null);
  const attachmentOptions = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!attachmentMenu) return;
    const close = (event: PointerEvent) => {
      if (!attachmentButton.current?.contains(event.target as Node)
        && !attachmentOptions.current?.contains(event.target as Node)) setAttachmentMenu(false);
    };
    document.addEventListener("pointerdown", close);
    return () => document.removeEventListener("pointerdown", close);
  }, [attachmentMenu]);
  function addAttachments(chosen: Attachment[]) {
    // The user can remove a token while a native picker or decoder is open.
    // Merge into the current draft instead of restoring that older selection.
    const current = latest.current.attachments;
    const added = chosen.filter((item) => !current.some((old) =>
      "path" in item && "path" in old && item.path === old.path));
    const next = [...current, ...added];
    if (next.length > 16 || next.filter((item) => "data" in item).length > 4)
      throw new Error("Attach up to four images and sixteen total attachments per message.");
    latest.current.onAttachments(next);
  }
  async function attachLocal(kind: "files" | "folder") {
    if (busy || props.running) return;
    setAttachmentMenu(false);
    setBusy(true);
    setError(undefined);
    try {
      const chosen = await desktop.chooseAttachments(kind);
      if (!chosen || !mounted.current) return;
      addAttachments(chosen);
    } catch (cause) {
      if (mounted.current) setError(String(cause));
    } finally {
      if (mounted.current) {
        setBusy(false);
        input.current?.focus();
      }
    }
  }
  function command(id: string) {
    change("");
    if (commands.find((item) => item.id === id)?.skill)
      // A skill that fails to load is this message's problem, shown here.
      void Promise.resolve(props.onSkill?.(id)).catch((cause) => setError(`Couldn't use ${id}: ${String(cause)}`));
    else if (id === "model" || id === "reasoning" || id === "mode")
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
  const menu = Boolean(props.control) || commands.length > 0;
  useLayoutEffect(() => {
    if (!menu) return setRoom(undefined);
    const measure = () => {
      if (!box.current) return;
      const edge = box.current.getBoundingClientRect();
      // The page the composer sits in is what a menu must stay inside: it ends above the status line.
      const page = box.current.closest(".main")?.getBoundingClientRect();
      const above = edge.top - (page?.top ?? 0) - HEAD - MARGIN;
      const below = (page?.bottom ?? window.innerHeight) - edge.bottom - MARGIN;
      setRoom({ down: below > above, most: Math.max(above, below, SMALLEST) });
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [menu]);
  // The arrow keys move through the commands; the one they are on stays in view.
  const menuList = useRef<HTMLDivElement>(null);
  useEffect(() => {
    menuList.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [commandIndex]);

  return (
    <div className="composer-wrap">
      <form
        ref={box}
        style={room && ({ "--menu-room": `${room.most}px` } as CSSProperties)}
        className={`composer ${dragging ? "dragging" : ""} ${room?.down ? "menus-down" : ""}`}
        onSubmit={(event) => {
          event.preventDefault();
          if (canSend) props.onSend();
        }}
        onKeyDownCapture={(event) => {
          if (event.key === "Escape" && attachmentMenu) {
            event.preventDefault();
            event.stopPropagation();
            setAttachmentMenu(false);
            attachmentButton.current?.focus();
          }
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
        <MessageTokens
          tokens={tokens}
          armed={armed}
          leaving={leaving}
          onRemove={removeToken}
          onLayout={setLayout}
        />
        <span className="composer-announce" aria-live="polite">
          {armedToken ? `Press Backspace again to remove ${armedToken.label}` : ""}
        </span>
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
          style={{
            textIndent: layout.indent || undefined,
            paddingTop: layout.lift ? `calc(var(--s-4) + ${layout.lift}px)` : undefined,
          }}
          // The indent slides, which rewraps the text; size it once it settles.
          onTransitionEnd={fit}
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
            const box = event.currentTarget;
            if (event.key === "Backspace" && tokens.length && box.selectionStart === 0 && box.selectionEnd === 0) {
              // First press marks the token nearest the text; a second removes it.
              event.preventDefault();
              const last = tokens[tokens.length - 1];
              if (armed === last.id) removeToken(last.id);
              else setArmed(last.id);
              return;
            }
            if (armed) setArmed(null);
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
          <div className="slash-menu" ref={menuList} role="listbox" aria-label="Chat commands">
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
                <span>{item.skill ? `Use skill · ${item.description}` : item.description}</span>
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
            ref={attachmentButton}
            type="button"
            className="icon-btn"
            onClick={() => setAttachmentMenu(!attachmentMenu)}
            disabled={busy || props.running}
            title="Attach images, files or a folder"
            aria-label="Attach files"
            aria-expanded={attachmentMenu}
          >
            <Icon name="clip" />
          </button>
          {attachmentMenu && <div ref={attachmentOptions} className="attachment-options" role="group" aria-label="Add attachments">
            <button type="button" onClick={() => { setAttachmentMenu(false); chooser.current?.click(); }}><Icon name="file" />Images</button>
            <button type="button" onClick={() => void attachLocal("files")}><Icon name="file" />Files</button>
            <button type="button" onClick={() => void attachLocal("folder")}><Icon name="folder" />Folder</button>
            <p>Files and folders are shared by local path. Medha may ask for access.</p>
          </div>}
          {(["model", "reasoning", "mode"] as Control[]).map((control) => (
            <button
              type="button"
              key={control}
              className={`chip ${props.control === control ? "active" : ""}${control === "model" && props.onConnect ? " needs" : ""}`}
              aria-haspopup="dialog"
              aria-expanded={props.control === control}
              onClick={() =>
                control === "model" && props.onConnect
                  ? props.onConnect()
                  : props.onControl(props.control === control ? null : control)
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
                  ? props.onConnect
                    ? "No model yet"
                    : props.settings?.profile || props.model || "Model"
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
          {props.settings?.profiles.find((profile) => profile.name === props.settings?.profile)?.chatgpt_plan && <PlanLine />}
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
              {props.control === "mode" && (
                <div className="popover-body">
                  {MODES.map((mode) => (
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
                </div>
              )}
              {props.control === "reasoning" && (
                <div className="popover-body">
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
                </div>
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

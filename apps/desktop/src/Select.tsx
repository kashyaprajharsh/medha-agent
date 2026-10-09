import {
  Children,
  isValidElement,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { useWorkspace } from "./Workspace";
import { Icon } from "./Icon";
import { ranked } from "./fuzzy";

function plain(node: ReactNode): string {
  return Children.toArray(node)
    .map((child) =>
      isValidElement<{ children?: ReactNode }>(child)
        ? plain(child.props.children)
        : String(child),
    )
    .join("");
}
/**
 * A quiet, keyboard-accessible picker shared by chat and settings. A long list
 * passes `search`, the words shown in an empty search box, to be typed into.
 */
export function Select({
  children,
  value,
  onChange,
  disabled,
  id,
  className,
  search,
  "aria-label": label,
}: {
  children: ReactNode;
  value: string | number;
  onChange: (event: { target: { value: string } }) => void;
  disabled?: boolean;
  id?: string;
  className?: string;
  search?: string;
  "aria-label"?: string;
}) {
  const workspace = useWorkspace();
  const items = Children.toArray(children)
    .filter(
      isValidElement<{
        value?: string | number;
        disabled?: boolean;
        children?: ReactNode;
      }>,
    )
    .map((child) => ({
      value: String(child.props.value ?? plain(child.props.children)),
      label: plain(child.props.children),
      disabled: child.props.disabled,
    }));
  const trigger = useRef<HTMLButtonElement>(null),
    menu = useRef<HTMLDivElement>(null),
    panel = useRef<HTMLDivElement>(null);
  const menuId = useId();
  const scrollSelection = useRef(false);
  const [open, setOpen] = useState(false),
    [index, setIndex] = useState(0),
    [query, setQuery] = useState("");
  // A searched list drops its "Choose…" row: the search box already says what to do.
  const pool =
    search === undefined ? items : items.filter((item) => item.value !== "");
  const shown =
    search === undefined ? items : ranked(pool, query, (item) => item.label);
  const [position, setPosition] = useState({
    left: 0,
    top: 0,
    width: 0,
    height: 280,
    above: false,
    bottom: 0,
  });
  useEffect(() => {
    if (!workspace.active || disabled) setOpen(false);
  }, [workspace.active, disabled]);
  const selected = items.findIndex((item) => item.value === String(value));
  function show() {
    scrollSelection.current = true;
    setQuery("");
    setIndex(
      Math.max(
        0,
        pool.findIndex((item) => item.value === String(value)),
      ),
    );
    setOpen(true);
  }
  function choose(next: number) {
    const item = shown[next];
    if (!item || item.disabled) return;
    onChange({ target: { value: item.value } });
    setOpen(false);
    trigger.current?.focus();
  }
  /** Keys that move through the list; `typing` leaves letters and the caret to the search box. */
  function onKey(event: KeyboardEvent, typing: boolean) {
    const moves = typing
      ? ["ArrowDown", "ArrowUp"]
      : ["ArrowDown", "ArrowUp", "Home", "End"];
    if (moves.includes(event.key)) {
      event.preventDefault();
      if (!open) {
        show();
        return;
      }
      if (!shown.length) return;
      const direction = event.key === "ArrowUp" ? -1 : 1;
      let next =
        event.key === "Home"
          ? 0
          : event.key === "End"
            ? shown.length - 1
            : (index + direction + shown.length) % shown.length;
      for (let count = 0; shown[next]?.disabled && count < shown.length; count++)
        next = (next + direction + shown.length) % shown.length;
      scrollSelection.current = true;
      setIndex(next);
    } else if (event.key === "Escape" && open) {
      event.preventDefault();
      event.stopPropagation();
      setOpen(false);
      trigger.current?.focus();
    } else if (event.key === "Enter" || (event.key === " " && !typing)) {
      event.preventDefault();
      open ? choose(index) : show();
    } else if (event.key === "Tab") {
      setOpen(false);
      if (typing) trigger.current?.focus();
    } else if (
      !typing &&
      event.key.length === 1 &&
      !event.metaKey &&
      !event.ctrlKey
    ) {
      const match = shown.findIndex(
        (item, at) =>
          at > index &&
          !item.disabled &&
          item.label.toLowerCase().startsWith(event.key.toLowerCase()),
      );
      if (match >= 0) {
        scrollSelection.current = true;
        setIndex(match);
        setOpen(true);
      }
    }
  }
  useLayoutEffect(() => {
    if (!open || !trigger.current) return;
    const place = () => {
      const rect = trigger.current!.getBoundingClientRect();
      const width = Math.min(Math.max(rect.width, 200), window.innerWidth - 24);
      const roomAbove = Math.max(0, rect.top - 18);
      const roomBelow = Math.max(0, window.innerHeight - rect.bottom - 18);
      // Sized for the whole list, so the menu stays put while a search narrows it.
      const tallest = search === undefined ? 280 : 340;
      const above =
        roomBelow <
          Math.min(
            items.length * 38 + 14 + (search === undefined ? 0 : 48),
            tallest,
          ) && roomAbove > roomBelow;
      const height = Math.min(tallest, above ? roomAbove : roomBelow);
      setPosition({
        left: Math.max(12, Math.min(rect.left, window.innerWidth - width - 12)),
        top: rect.bottom + 6,
        bottom: window.innerHeight - rect.top + 6,
        above,
        width,
        height,
      });
    };
    const onScroll = (event: Event) => {
      // Scrolling the menu must not reposition the menu beneath the pointer.
      if (event.target instanceof Node && menu.current?.contains(event.target))
        return;
      place();
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [open, items.length]);
  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => {
      if (
        !trigger.current?.contains(event.target as Node) &&
        !(panel.current ?? menu.current)?.contains(event.target as Node)
      )
        setOpen(false);
    };
    document.addEventListener("pointerdown", close);
    return () => document.removeEventListener("pointerdown", close);
  }, [open]);
  useEffect(() => {
    if (!open || !scrollSelection.current) return;
    const list = menu.current;
    const option = list?.children[index] as HTMLElement | undefined;
    if (!list || !option) return;
    const box = list.getBoundingClientRect();
    const row = option.getBoundingClientRect();
    if (row.top < box.top + 6) list.scrollTop += row.top - box.top - 6;
    else if (row.bottom > box.bottom - 6)
      list.scrollTop += row.bottom - box.bottom + 6;
  }, [index, open, query]);
  const place = {
    left: position.left,
    top: position.above ? undefined : position.top,
    bottom: position.above ? position.bottom : undefined,
    width: position.width,
    maxHeight: position.height,
  };
  const list = (
    <div
      ref={menu}
      id={menuId}
      role="listbox"
      aria-label={label}
      className={search === undefined ? "select-menu" : "select-options"}
      style={search === undefined ? place : undefined}
      onPointerDown={(event) => event.stopPropagation()}
      onClick={(event) => event.stopPropagation()}
    >
      {shown.map((item, at) => (
        <button
          type="button"
          tabIndex={-1}
          disabled={item.disabled}
          id={`${menuId}-${at}`}
          key={item.value}
          role="option"
          aria-selected={item.value === String(value)}
          aria-disabled={item.disabled}
          className={at === index ? "focused" : ""}
          onPointerMove={() => {
            scrollSelection.current = false;
            if (!item.disabled) setIndex(at);
          }}
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => choose(at)}
        >
          <span>{item.label}</span>
          {item.value === String(value) && <Icon name="check" />}
        </button>
      ))}
      {!shown.length && <p className="select-empty">Nothing matches.</p>}
    </div>
  );
  return (
    <>
      <button
        ref={trigger}
        id={id}
        type="button"
        role="combobox"
        aria-label={label}
        aria-expanded={open}
        aria-controls={menuId}
        aria-haspopup="listbox"
        aria-activedescendant={open ? `${menuId}-${index}` : undefined}
        disabled={disabled}
        className={`select-trigger ${className || ""}`}
        onClick={() => (open ? setOpen(false) : show())}
        onKeyDown={(event) => onKey(event, false)}
      >
        <span>{items[selected]?.label || "Choose…"}</span>
        <Icon name="down" />
      </button>
      {open &&
        createPortal(
          search === undefined ? (
            list
          ) : (
            <div
              ref={panel}
              className="select-menu searching"
              style={place}
              onPointerDown={(event) => event.stopPropagation()}
              onClick={(event) => event.stopPropagation()}
            >
              <div className="model-search">
                <Icon name="search" />
                <input
                  autoFocus
                  value={query}
                  placeholder={search}
                  aria-label={search}
                  aria-controls={menuId}
                  aria-activedescendant={
                    shown.length ? `${menuId}-${index}` : undefined
                  }
                  onChange={(event) => {
                    scrollSelection.current = true;
                    setQuery(event.target.value);
                    setIndex(0);
                  }}
                  onKeyDown={(event) => onKey(event, true)}
                />
              </div>
              {list}
            </div>
          ),
          document.body,
        )}
    </>
  );
}

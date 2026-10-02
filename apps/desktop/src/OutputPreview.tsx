import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Icon, type IconName } from "./Icon";
import { SourceCode } from "./Markdown";
import { useWorkspaceApi } from "./Workspace";
import { hasScripts, labelOf, noteOf, slidesOf, versionsOf, type Output, type OutputKind } from "./outputBlocks";
import { useOutputs, type Detail } from "./outputContext";
import { useKeyed } from "./outputFiles";
import { unreachable } from "./outputFrame";
import { Gallery } from "./OutputGallery";
import { Unshown } from "./OutputPlate";
import { Rendered } from "./OutputRendered";

const LANGUAGE: Partial<Record<OutputKind, string>> = {
  diagram: "mermaid",
  drawing: "xml",
  page: "xml",
  slides: "xml",
};
const EXTENSION: Partial<Record<OutputKind, string>> = {
  app: "json",
  diagram: "mmd",
  drawing: "svg",
  page: "html",
  slides: "html",
};
const ZOOMS = [0.5, 0.75, 1, 1.5, 2, 3];
const OFFLINE = "It can't reach the internet or your files.";

function Tool(props: { icon: IconName; label: string; onClick: () => void; pressed?: boolean; disabled?: boolean }) {
  return (
    <button
      type="button"
      className="icon-btn"
      aria-label={props.label}
      title={props.label}
      aria-pressed={props.pressed}
      disabled={props.disabled}
      onClick={props.onClick}
    >
      <Icon name={props.icon} />
    </button>
  );
}

/** Grows the viewer's content out of the plate that was clicked, once. */
function useGrow(canvas: React.RefObject<HTMLDivElement | null>, anchor: string | undefined) {
  const { from } = useOutputs();
  useLayoutEffect(() => {
    const start = from?.current;
    const box = canvas.current;
    if (from) from.current = null;
    if (!start || !box?.animate || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const end = box.getBoundingClientRect();
    if (!end.width || !start.width) return;
    const style = getComputedStyle(box);
    const grow = box.animate(
      [
        {
          transformOrigin: "0 0",
          transform: `translate(${start.left - end.left}px, ${start.top - end.top}px) scale(${start.width / end.width})`,
          opacity: 0.4,
        },
        { transformOrigin: "0 0", transform: "none", opacity: 1 },
      ],
      {
        duration: parseFloat(style.getPropertyValue("--t-slow")) || 320,
        easing: style.getPropertyValue("--ease-out").trim() || "ease-out",
      },
    );
    return () => grow.cancel();
  }, [anchor, canvas, from]);
}

/** The open output at full size, pinned beside the chat. */
export function Preview() {
  const { open } = useOutputs();
  return open ? <Viewer open={open} /> : <Gallery />;
}

function Viewer({ open }: { open: Output }) {
  const api = useWorkspaceApi();
  const { all, stopped, online, stop, allow, follow, browse } = useOutputs();
  const [code, setCode] = useState(false);
  const [large, setLarge] = useState(false);
  const shown = `${open.source}\0${open.path}\0${open.screen ? open.anchor : ""}`;
  const [problem, setProblem] = useKeyed<string>(shown);
  const [detail, setDetail] = useKeyed<Detail>(shown);
  const [zoom, setZoom] = useState<number>();
  const [slide, setSlide] = useState(0);
  const [copied, setCopied] = useState(false);
  const canvas = useRef<HTMLDivElement>(null);
  const { anchor, kind, source, path, name } = open;
  useEffect(() => {
    setCode(false);
    setZoom(undefined);
    setSlide(0);
    setCopied(false);
  }, [anchor]);
  useGrow(canvas, anchor);

  const slides = kind === "slides" || kind === "deck";
  const step = useCallback((by: number) => setSlide((at) => Math.max(0, at + by)), []);
  useEffect(() => {
    if (!large && !slides) return;
    const key = (event: KeyboardEvent) => {
      if (event.key === "Escape" && large) setLarge(false);
      else if (slides && event.key === "ArrowRight") step(1);
      else if (slides && event.key === "ArrowLeft") step(-1);
      else return;
      event.preventDefault();
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [large, slides, step]);

  const versions = versionsOf(all, open);
  const version = versions.findIndex((other) => other.anchor === anchor);
  const newest = versions.at(-1)!;
  const label = labelOf(open);
  // What a screen was given to draw stands in for its source when copied or saved.
  const text = open.screen ? JSON.stringify(open.screen.result ?? {}, null, 2) : source;
  const scripted = (kind === "page" && hasScripts(source)) || undefined;
  const live = !stopped.includes(anchor);
  const web = online.includes(anchor);
  const page = kind === "page" && !path;
  const wanted = page ? unreachable(source) : [];
  const note = noteOf(open);
  const touch = web
    ? "Can reach the internet. It still can't see your files or the rest of Medha."
    : wanted.length
      ? `Offline. It asked for ${wanted.slice(0, 3).join(", ")}${wanted.length > 3 ? " and more" : ""}, which it can't reach.`
      : page
        ? `Runs here, offline. ${OFFLINE}`
        : slides && !path
          ? `Shown here, offline. ${OFFLINE}`
          : undefined;

  const copy = () =>
    void navigator.clipboard
      .writeText(path ?? text)
      .then(() => setCopied(true))
      .catch(() => {});
  const save = () =>
    void api
      .saveOutput(
        path ? (path.split(/[\\/]/).at(-1) ?? name) : `${name}.${EXTENSION[kind] ?? "txt"}`,
        path ? { path } : { text },
      )
      .catch(() => {});
  const powerpoint = () =>
    void import("./outputDeckWrite")
      .then((writer) => writer.writeDeck(slidesOf(source), name))
      .then((bytes) => api.saveOutput(`${name}.pptx`, { bytes }))
      .catch(() => {});
  const zoomBy = (by: number) => {
    const at = ZOOMS.indexOf(zoom ?? 1);
    setZoom(ZOOMS[Math.min(ZOOMS.length - 1, Math.max(0, at + by))]);
  };

  const viewer = (
    <div className={`out-viewer ${large ? "large" : ""}`}>
      <header className="out-bar">
        <Tool icon="blocks" label="All outputs" onClick={() => browse?.()} />
        <div className="out-title">
          <b>{name}</b>
          {name !== label && <span>{label}</span>}
        </div>
        {!path && LANGUAGE[kind] && (
          <div className="out-switch" role="group" aria-label="View">
            <button type="button" aria-pressed={!code} onClick={() => setCode(false)}>
              Preview
            </button>
            <button type="button" aria-pressed={code} onClick={() => setCode(true)}>
              Code
            </button>
          </div>
        )}
        {scripted && (
          <button type="button" className="out-toggle" onClick={() => stop?.(open, live)}>
            <Icon name={live ? "stop" : "play"} />
            {live ? "Stop" : "Run"}
          </button>
        )}
        <div className="out-tools">
          {!code && (kind === "image" || kind === "diagram" || kind === "drawing") && (
            <>
              <Tool icon="zoomout" label="Zoom out" onClick={() => zoomBy(-1)} />
              <button type="button" className="out-fit" onClick={() => setZoom(undefined)}>
                {zoom ? `${Math.round(zoom * 100)}%` : "Fit"}
              </button>
              <Tool icon="zoomin" label="Zoom in" onClick={() => zoomBy(1)} />
              <i className="out-rule" />
            </>
          )}
          {kind === "slides" && !path && (
            <button type="button" className="out-fit" onClick={powerpoint}>
              Save as PowerPoint
            </button>
          )}
          {slides && <Tool icon="present" label="Present" onClick={() => setLarge(true)} />}
          <Tool icon={copied ? "check" : "copy"} label={path ? "Copy path" : "Copy"} onClick={copy} />
          <Tool icon="save" label="Save a copy" onClick={save} />
          <Tool
            icon={large ? "x" : "maximize"}
            label={large ? "Close large view" : "Open large"}
            onClick={() => setLarge(!large)}
          />
        </div>
      </header>
      <div className="out-room">
        {versions.length > 1 && (
          <div className="out-versions" role="group" aria-label="Versions">
            {versions.map((other, index) => (
              <button
                type="button"
                key={other.anchor}
                aria-pressed={index === version}
                aria-label={`Version ${index + 1}`}
                onClick={() => follow?.(other)}
              >
                v{index + 1}
              </button>
            ))}
            {newest.anchor !== anchor && (
              <button type="button" className="out-ready" onClick={() => follow?.(newest)}>
                v{versions.length} is ready
              </button>
            )}
          </div>
        )}
      <div className={`out-canvas kind-${kind}`} ref={canvas}>
        {code ? (
          <SourceCode text={source} language={LANGUAGE[kind]} />
        ) : problem && path ? (
          <Unshown output={open} problem={problem} />
        ) : problem && open.screen ? (
          <div className="out-problem">
            <b>{label}'s screen isn't available</b>
            <span>{problem}</span>
          </div>
        ) : problem ? (
          <div className="out-problem">
            <Icon name="alert" />
            <b>This {label.toLowerCase()} couldn't be drawn</b>
            <span>{problem}</span>
          </div>
        ) : (
          <Rendered
            output={open}
            size="full"
            zoom={zoom}
            slide={slide}
            onSlide={setSlide}
            onProblem={setProblem}
            onDetail={setDetail}
          />
        )}
      </div>
      </div>
      <footer className="out-foot">
        {note && <p>{note}</p>}
        {detail?.foot && <p>{detail.foot}</p>}
        {touch && (
          <p>
            <Icon name={web ? "globe" : "lock"} />
            {touch}
            {(web || wanted.length > 0) && (
              <button type="button" className="out-allow" onClick={() => allow?.(open, !web)}>
                {web ? "Go offline" : "Allow internet for this page"}
              </button>
            )}
          </p>
        )}
      </footer>
    </div>
  );
  // The panel clips whatever is inside it, so the large view is drawn over the whole window.
  return large ? createPortal(viewer, document.body) : viewer;
}

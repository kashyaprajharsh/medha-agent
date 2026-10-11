import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import { hasScripts, slidePage, slidesOf, type Deck } from "./outputBlocks";
import { useOutputs, type RenderProps } from "./outputContext";
import { useNear, useWidth } from "./outputFiles";
import { framed } from "./outputFrame";

const SLIDE = { width: 960, height: 540 };
const SCREEN = { width: 1120, height: 700 };

type FrameProps = {
  html: string;
  run: boolean;
  /** The person allowed this page the web; never set otherwise. */
  online?: boolean;
  title: string;
  onProblem?: (problem: string | undefined) => void;
  bundle?: string;
  onBlocked?: (resource: string, directive: string) => void;
};

/** A page in a frame of its own: no files, no way to the app around it, and no network unless allowed. */
export function Frame({ html, run, online = false, title, onProblem, bundle, onBlocked }: FrameProps) {
  const api = useWorkspaceApi();
  const box = useRef<HTMLDivElement>(null);
  // Alive only near the screen, so a long chat never keeps dozens of pages running.
  const near = useNear(box, false);
  const [url, setUrl] = useState<string>();
  useEffect(() => {
    if (!near) return;
    let current = true;
    let made: string | undefined;
    void framed(html, run, online)
      .then((page) => api.screenPut(page, run, online, undefined, bundle))
      .then((link) => {
        if (!current) return void api.screenDrop(link);
        made = link;
        setUrl(link);
        onProblem?.(undefined);
      })
      .catch((cause) => current && onProblem?.(String(cause)));
    return () => {
      current = false;
      if (made) void api.screenDrop(made);
    };
  }, [api, html, run, online, near, onProblem, bundle]);
  useEffect(() => {
    if (!url) return;
    const id = url.split("/screen/")[1]?.split("/")[0];
    const hear = (event: MessageEvent) => {
      const data = event.data;
      if (event.source === box.current?.querySelector("iframe")?.contentWindow && data?.kind === "medha-preview-blocked" && data.id === id && typeof data.uri === "string" && typeof data.directive === "string") onBlocked?.(data.uri.slice(0, 300), data.directive.slice(0, 80));
    };
    window.addEventListener("message", hear);
    return () => window.removeEventListener("message", hear);
  }, [url, onBlocked]);
  return (
    <div className="out-frame" ref={box}>
      {url && near && (
        <iframe
          key={url}
          src={url}
          title={title}
          sandbox="allow-scripts"
          referrerPolicy="no-referrer"
        />
      )}
    </div>
  );
}

export function Page({ output, size, writing, onProblem, onDetail }: RenderProps) {
  const { stopped, online, stop } = useOutputs();
  const [blocked, setBlocked] = useState<string[]>([]);
  const blockedAt = useCallback((uri: string, directive: string) => setBlocked((list) => list.includes(`${uri} (${directive})`) ? list : [...list, `${uri} (${directive})`].slice(-8)), []);
  useEffect(() => { setBlocked([]); }, [output.anchor, online.includes(output.anchor), stopped.includes(output.anchor)]);
  useEffect(() => {
    const notices = [...(output.missing?.length ? [`Missing local assets: ${output.missing.join(", ")}.`] : []), ...(blocked.length ? [`Blocked preview resources: ${blocked.join(", ")}.`] : [])];
    onDetail?.({ foot: notices.length ? notices.join(" ") : undefined });
  }, [output.missing, blocked, onDetail]);
  if (writing)
    return <pre className="out-source">{output.source.split("\n").slice(-12).join("\n")}</pre>;
  const scripted = hasScripts(output.source);
  // A page runs as written unless the person stopped it: many are styled by their scripts.
  const live = scripted && !stopped.includes(output.anchor);
  const full = size === "full";
  const halted = stopped.includes(output.anchor);
  const frame = (
    <Frame
      html={output.source}
      run={live}
      online={!halted && online.includes(output.anchor)}
      title={output.name}
      onProblem={onProblem}
      bundle={output.bundle}
      onBlocked={blockedAt}
    />
  );
  return (
    <div className={`out-page ${scripted && !live ? "still" : ""} ${live && full ? "live" : ""}`}>
      {full ? (
        <div className="out-page-fit">{frame}</div>
      ) : (
        // In the chat a page is a picture of its first screen; it is used in the viewer.
        <Fit className="out-page-fit" base={SCREEN.width} height={SCREEN.height}>
          {frame}
        </Fit>
      )}
      {scripted && !live && stop && size !== "thumb" && (
        <button
          type="button"
          className="out-run"
          onClick={(event) => {
            event.stopPropagation();
            stop(output, false);
          }}
        >
          <Icon name="play" />
          Run
        </button>
      )}
      {size !== "thumb" && (blocked.length > 0 || !!output.missing?.length) && <p className="out-notice">{output.missing?.length ? `Missing local assets: ${output.missing.join(", ")}. ` : ""}{blocked.length > 0 ? "Some preview resources were blocked. Internet access can be allowed in Preview." : ""}</p>}
    </div>
  );
}

/** Draws its content at a width it was made for, then scales it to the room there is. */
function Fit({ className, base, height, children }: { className: string; base: number; height: number; children: ReactNode }) {
  const [box, width] = useWidth();
  return (
    <div className={className} ref={box}>
      {width > 0 && (
        <div className="out-fit-page" style={{ width: base, height, scale: width / base }}>
          {children}
        </div>
      )}
    </div>
  );
}

function Slide({ page, title, ...reach }: Omit<FrameProps, "html" | "run"> & { page: string; run?: boolean }) {
  return (
    <Fit className="out-slide" base={SLIDE.width} height={SLIDE.height}>
      <Frame html={page} run={reach.run ?? false} title={title} {...reach} />
    </Fit>
  );
}

/** A deck: written by Medha as a page of sections, or read from a PowerPoint file. */
export function Slides({ output, size, writing, slide = 0, onSlide, onDetail, onProblem, read }: RenderProps & { read?: Deck }) {
  const { stopped, online } = useOutputs();
  const deck = useMemo(() => read ?? slidesOf(output.source), [read, output.source]);
  const pages = useMemo(() => deck.slides.map((_, index) => slidePage(deck, index)), [deck]);
  const count = pages.length;
  const at = Math.min(slide, count - 1);
  const [blocked, setBlocked] = useState<string[]>([]);
  const blockedAt = useCallback((uri: string, directive: string) => setBlocked((list) => list.includes(`${uri} (${directive})`) ? list : [...list, `${uri} (${directive})`].slice(-8)), []);
  const web = online.includes(output.anchor) && !stopped.includes(output.anchor);
  const run = hasScripts(output.source) && !stopped.includes(output.anchor);
  const reach = { online: web, run, bundle: output.bundle, onProblem, onBlocked: blockedAt };
  useEffect(() => { setBlocked([]); }, [output.anchor, web]);
  useEffect(() => {
    onDetail?.({ caption: String(deck.total ?? count), foot: [`Slide ${at + 1} of ${deck.total ?? count}.`, ...(deck.notices ?? []), ...(output.missing?.length ? [`Missing local assets: ${output.missing.join(", ")}.`] : []), ...(blocked.length ? [`Blocked preview resources: ${blocked.join(", ")}.`] : [])].join(" ") });
  }, [onDetail, count, at, deck, output.missing, blocked]);
  if (size !== "full" || !onSlide)
    // In the chat a deck shows each slide as it arrives, then settles on its first.
    return <Slide page={pages[writing ? count - 1 : 0]} title={output.name} {...reach} />;
  return (
    <div className="out-deck">
      <div className="out-deck-stage">
        <Slide page={pages[at]} title={`${output.name}, slide ${at + 1}`} {...reach} />
        <button type="button" aria-label="Previous slide" disabled={at === 0} onClick={() => onSlide(at - 1)}>
          <Icon name="chev" />
        </button>
        <button type="button" aria-label="Next slide" disabled={at >= count - 1} onClick={() => onSlide(at + 1)}>
          <Icon name="chev" />
        </button>
      </div>
      <div className="out-strip" role="tablist" aria-label="Slides">
        {pages.map((page, index) => (
          <button
            type="button"
            role="tab"
            key={index}
            aria-selected={index === at}
            aria-label={`Slide ${index + 1}`}
            onClick={() => onSlide(index)}
          >
            <Slide page={page} title={`Slide ${index + 1}`} bundle={output.bundle} onProblem={onProblem} />
          </button>
        ))}
      </div>
    </div>
  );
}

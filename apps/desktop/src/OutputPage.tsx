import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
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
};

/** A page in a frame of its own: no files, no way to the app around it, and no network unless allowed. */
export function Frame({ html, run, online = false, title, onProblem }: FrameProps) {
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
      .then((page) => api.screenPut(page, run, online))
      .then((link) => {
        if (!current) return void api.screenDrop(link);
        made = link;
        setUrl(link);
      })
      .catch((cause) => current && onProblem?.(String(cause)));
    return () => {
      current = false;
      if (made) void api.screenDrop(made);
    };
  }, [api, html, run, online, near, onProblem]);
  return (
    <div className="out-frame" ref={box}>
      {url && near && (
        <iframe
          key={url}
          src={url}
          title={title}
          sandbox={run || online ? "allow-scripts" : ""}
          referrerPolicy="no-referrer"
        />
      )}
    </div>
  );
}

export function Page({ output, size, writing, onProblem }: RenderProps) {
  const { stopped, online, stop } = useOutputs();
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

function Slide({ page, title }: { page: string; title: string }) {
  return (
    <Fit className="out-slide" base={SLIDE.width} height={SLIDE.height}>
      <Frame html={page} run={false} title={title} />
    </Fit>
  );
}

/** A deck: written by Medha as a page of sections, or read from a PowerPoint file. */
export function Slides({ output, size, writing, slide = 0, onSlide, onDetail, read }: RenderProps & { read?: Deck }) {
  const deck = useMemo(() => read ?? slidesOf(output.source), [read, output.source]);
  const pages = useMemo(() => deck.slides.map((_, index) => slidePage(deck, index)), [deck]);
  const count = pages.length;
  const at = Math.min(slide, count - 1);
  useEffect(() => {
    onDetail?.({ caption: String(count), foot: `Slide ${at + 1} of ${count}.` });
  }, [onDetail, count, at]);
  if (size !== "full" || !onSlide)
    // In the chat a deck shows each slide as it arrives, then settles on its first.
    return <Slide page={pages[writing ? count - 1 : 0]} title={output.name} />;
  return (
    <div className="out-deck">
      <div className="out-deck-stage">
        <Slide page={pages[at]} title={`${output.name}, slide ${at + 1}`} />
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
            <Slide page={page} title={`Slide ${index + 1}`} />
          </button>
        ))}
      </div>
    </div>
  );
}

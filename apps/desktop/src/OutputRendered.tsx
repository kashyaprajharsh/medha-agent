import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";
import { useWorkspace } from "./Workspace";
import { growing, lastLine, slidesOf, type Deck } from "./outputBlocks";
import type { RenderProps } from "./outputContext";
import { useFile } from "./outputFiles";
import { AppScreen } from "./OutputApp";
import { Picture, Sound, Video } from "./OutputMedia";
import { Page, Slides } from "./OutputPage";
import { drawDiagram, drawingUrl } from "./outputRender";

const Paper = lazy(() => import("./OutputPaper"));
const BEAT = 200;

/** Any output, drawn: the same call for a plate in the chat, a thumbnail and the viewer. */
export function Rendered(props: RenderProps) {
  const { kind, path } = props.output;
  if (kind === "app") return <AppScreen {...props} />;
  if (kind === "image") return <Picture {...props} />;
  if (kind === "video") return <Video {...props} />;
  if (kind === "audio") return <Sound {...props} />;
  if (kind === "pdf" || kind === "document" || kind === "table")
    return (
      <Suspense fallback={<div className="out-wait" />}>
        <Paper {...props} />
      </Suspense>
    );
  if (kind === "deck") return path?.toLowerCase().endsWith(".pptx") ? <DeckFile {...props} /> : <Unopened {...props} />;
  if (path) return <FromFile {...props} />;
  if (kind === "diagram") return <Diagram {...props} />;
  if (kind === "drawing") return <Drawing {...props} />;
  if (kind === "slides") return <Slides {...props} />;
  return <Page {...props} />;
}

/** A diagram, a drawing or a page that Medha saved as a file is drawn from the file's text. */
function FromFile(props: RenderProps) {
  const { output } = props;
  const file = useFile(output.path, props.onProblem);
  const read = useMemo(() => {
    if (file?.text === undefined) return undefined;
    const deck = output.kind === "page" && slidesOf(file.text).slides.length > 1;
    return { ...output, kind: deck ? ("slides" as const) : output.kind, source: file.text, path: undefined };
  }, [file, output]);
  return read ? <Rendered {...props} output={read} /> : <div className="out-wait" />;
}

/** A PowerPoint file, read into the same deck a page of sections makes. */
function DeckFile(props: RenderProps) {
  const { onProblem } = props;
  const file = useFile(props.output.path, onProblem);
  const [deck, setDeck] = useState<Deck>();
  useEffect(() => {
    if (!file?.bytes) return;
    let current = true;
    void import("./outputDeckRead")
      .then((reader) => reader.readDeck(new Uint8Array(file.bytes!)))
      .then((read) => current && setDeck(read))
      .catch(() => current && onProblem?.("These slides could not be read. They open in their own app."));
    return () => {
      current = false;
    };
  }, [file, onProblem]);
  return deck ? <Slides {...props} read={deck} /> : <div className="out-wait" />;
}

function Unopened({ onProblem }: RenderProps) {
  useEffect(() => onProblem?.("Slides in this format open in their own app."), [onProblem]);
  return <div className="out-wait" />;
}

function Diagram({ output, writing = false, zoom, onProblem }: RenderProps) {
  const { theme, look } = useWorkspace();
  const { source, name } = output;
  const [svg, setSvg] = useState<{ svg: string; paper?: boolean }>();
  const arriving = useRef(source);
  arriving.current = source;
  // While it is written it is redrawn on a steady beat from whatever has arrived.
  // Waiting for the text to pause would draw nothing: a streaming reply never pauses.
  useEffect(() => {
    if (!writing) return;
    let current = true;
    let drawnFor: string | undefined;
    let timer: ReturnType<typeof setTimeout>;
    const beat = async () => {
      const text = arriving.current;
      if (text !== drawnFor) {
        drawnFor = text;
        const whole = await drawDiagram(text, theme === "dark");
        const drawn = "problem" in whole ? await drawDiagram(growing(text), theme === "dark") : whole;
        if (current && "svg" in drawn) setSvg(drawn);
      }
      if (current) timer = setTimeout(() => void beat(), BEAT);
    };
    timer = setTimeout(() => void beat(), 0);
    return () => {
      current = false;
      clearTimeout(timer);
    };
  }, [writing, theme, look]);
  useEffect(() => {
    if (writing) return;
    let current = true;
    void drawDiagram(source, theme === "dark").then((drawn) => {
      if (!current) return;
      if ("svg" in drawn) setSvg(drawn);
      onProblem?.("problem" in drawn ? drawn.problem : undefined);
    });
    return () => {
      current = false;
    };
  }, [source, theme, look, writing, onProblem]);
  if (!svg) return <pre className="out-source">{source}</pre>;
  return (
    <>
      <div
        className={`out-diagram ${zoom ? "zoomed" : ""} ${svg.paper ? "on-paper" : ""}`}
        style={zoom ? { width: `${zoom * 100}%` } : undefined}
        role="img"
        aria-label={name}
        dangerouslySetInnerHTML={{ __html: svg.svg }}
      />
      {writing && <code className="out-tail">{lastLine(source)}</code>}
    </>
  );
}

function Drawing({ output, writing = false, zoom, onProblem }: RenderProps) {
  const { source, name } = output;
  const [url, setUrl] = useState<string>();
  useEffect(() => {
    if (writing) return;
    const made = drawingUrl(source);
    setUrl(made);
    onProblem?.(made ? undefined : "It has no <svg> element.");
    return () => {
      if (made) URL.revokeObjectURL(made);
    };
  }, [source, writing, onProblem]);
  if (!url) return <pre className="out-source">{source}</pre>;
  return (
    <img
      className="out-drawing"
      src={url}
      alt={name}
      style={zoom ? { width: `${zoom * 100}%`, maxWidth: "none" } : undefined}
      onError={() => onProblem?.("It is not a valid SVG.")}
    />
  );
}

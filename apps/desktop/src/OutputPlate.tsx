import { memo, useEffect, useMemo, useRef, useState } from "react";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import type { ToolScreen } from "./api";
import type { Step, WritingCall } from "./live";
import { useOffered } from "./OutputApp";
import { labelOf, noteOf, screenOutput, titleOf, versionsOf, type Output, type OutputKind } from "./outputBlocks";
import { useOutputs, type Detail } from "./outputContext";
import { useKeyed } from "./outputFiles";
import { Rendered } from "./OutputRendered";

type Props = {
  anchor: string;
  kind: OutputKind;
  source: string;
  writing: boolean;
  order: number;
  path?: string;
  name?: string;
  screen?: ToolScreen;
};

type ScreensProps = {
  steps: Step[];
  at: number;
  /** Calls the model is still writing; one to a tool with a screen is drawn as it arrives. */
  writing?: WritingCall[];
  fresh?: boolean;
};

/** The screens the tools in one stretch of work came with, each as a plate. */
export const Screens = memo(function Screens({ steps, at, writing, fresh }: ScreensProps) {
  const { made } = useOutputs();
  const offered = useOffered(writing);
  // One plate per call, from its first written piece to its result, so the
  // screen that drew the pieces is the one that shows the finished drawing.
  const shown = new Map<string, { screen: ToolScreen; written: boolean }>();
  for (const call of writing ?? []) {
    const tool = offered(call.tool);
    if (tool) shown.set(call.id, { screen: { ...tool, partial: call.text }, written: false });
  }
  for (const step of steps) if (step.screen) shown.set(step.id, { screen: step.screen, written: true });
  // A screen that arrives while the person watches opens beside the chat, like anything Medha makes.
  const newest = [...shown].filter(([, entry]) => entry.written).at(-1);
  const anchor = newest && `screen:${newest[0]}`;
  const arrived = newest?.[1].screen;
  useEffect(() => {
    if (fresh && arrived && anchor) made?.(screenOutput(anchor, arrived));
  }, [fresh, made, anchor, arrived]);
  if (!shown.size) return null;
  return (
    <div className="out-grid">
      {[...shown].map(([id, { screen, written }], index) => (
        <Plate
          key={id}
          {...screenOutput(`screen:${id}`, screen)}
          screen={screen}
          writing={!written}
          order={at * 1000 + index}
        />
      ))}
    </div>
  );
});

/** The kind and what was learned about it: "Slides, 6", "Image, 1600 × 900". */
export function Caption({ output, detail }: { output: Output; detail?: Detail }) {
  const label = labelOf(output);
  if (output.name === label && !detail?.caption) return null;
  return <span className="out-kind">{detail?.caption ? `${label}, ${detail.caption}` : label}</span>;
}

/** A file that is here but not shown: what it is, why, and the way to it. */
export function Unshown({ output, problem }: { output: Output; problem: string }) {
  const api = useWorkspaceApi();
  const path = output.path!;
  return (
    <div className="out-problem">
      <b>{path.split(/[\\/]/).at(-1)}</b>
      <span>{problem}</span>
      <div className="out-problem-actions">
        <button type="button" onClick={() => void api.revealFile(path).catch(() => {})}>
          Show in folder
        </button>
        <button type="button" onClick={() => void api.openFile(path).catch(() => {})}>
          Open
        </button>
      </div>
    </div>
  );
}

/**
 * A file the reply only named. It becomes a plate once it is found in the chat's
 * folder, and stays out of the way when it is not there or nothing can open it.
 */
export function Mentioned({ fresh, ...plate }: Props & { fresh: boolean }) {
  const api = useWorkspaceApi();
  const { enter, made } = useOutputs();
  const [there, setThere] = useState(false);
  const { anchor, kind, path = "", name } = plate;
  useEffect(() => {
    if (!enter) return;
    let current = true;
    const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
    void api
      .files(path.slice(0, Math.max(cut, 0)))
      .then((entries) => {
        if (!current || !entries.some((entry) => !entry.directory && entry.name === path.slice(cut + 1))) return;
        setThere(true);
        if (fresh) made?.({ anchor, kind, source: "", path, name: name ?? path.slice(cut + 1) });
      })
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [api, enter, made, fresh, anchor, kind, path, name]);
  return there ? <Plate {...plate} /> : null;
}

/** One thing Medha made, shown where it was made: the content, then one caption row. */
export const Plate = memo(function Plate({ anchor, kind, source, writing, order, path, name: given, screen }: Props) {
  const { open, all, viewing, show, follow, enter, made, ask } = useOutputs();
  const [problem, setProblem] = useKeyed<string>(`${source}\0${path}\0${screen?.resource}`);
  const [detail, setDetail] = useKeyed<Detail>(`${source}\0${path}\0${screen?.resource}`);
  const [code, setCode] = useState(false);
  const stage = useRef<HTMLDivElement>(null);
  const output = useMemo<Output>(
    () => ({ anchor, kind, source, path, screen, name: given ?? titleOf(kind, source) }),
    [anchor, kind, source, path, screen, given],
  );
  const { name } = output;
  useEffect(() => (writing ? undefined : enter?.(output, order)), [enter, output, order, writing]);
  const here = open?.anchor === anchor;
  useEffect(() => {
    if (here && open.source !== source) follow?.(output);
  }, [here, open, follow, output, source]);
  // The moment it finishes being written, it develops once.
  const wrote = useRef(writing);
  const [developed, setDeveloped] = useState(false);
  useEffect(() => {
    if (wrote.current && !writing) {
      setDeveloped(true);
      made?.(output);
    }
    wrote.current = writing;
  }, [writing, made, output]);

  const versions = versionsOf(all, output);
  const version = versions.findIndex((other) => other.anchor === anchor) + 1;
  // The same thing shown again elsewhere in the chat is current too; a screen is only ever itself.
  const current =
    here || (!screen && !!open && open.kind === kind && open.source === source && open.path === path);
  const reveal =
    show && !writing && !problem
      ? () => show(output, stage.current?.getBoundingClientRect())
      : undefined;
  const label = labelOf(output);
  const chip = versions.length > 1 && version > 0 && <span className="out-version">v{version}</span>;

  // The viewer already shows it, so a revision is one line and the chat stays short.
  if (reveal && viewing && version > 1)
    return (
      <button type="button" className={`out-revision ${current ? "current" : ""}`} onClick={reveal}>
        <span className="out-thumb">
          <Rendered output={output} size="thumb" onProblem={setProblem} />
        </span>
        <span className="out-revision-text">
          <b>{name}</b>
          <small>{noteOf(output) || `${label}, updated`}</small>
        </span>
        {chip}
      </button>
    );

  const shown = !problem || writing;
  return (
    <figure
      className={`out-plate kind-${kind} ${current ? "current" : ""} ${writing ? "writing" : ""} ${developed ? "developed" : ""}`}
    >
      <div
        ref={stage}
        className={`out-stage ${reveal ? "opens" : ""}`}
        onClick={reveal}
      >
        {shown ? (
          <Rendered
            output={output}
            size="plate"
            writing={writing}
            onProblem={setProblem}
            onDetail={setDetail}
          />
        ) : path ? (
          <Unshown output={output} problem={problem} />
        ) : screen ? (
          <div className="out-problem">
            <b>{label}'s screen isn't available</b>
            <span>{problem}</span>
          </div>
        ) : (
          <div className="out-problem">
            <Icon name="alert" />
            <b>This {label.toLowerCase()} couldn't be drawn</b>
            <span>{problem}</span>
            <div className="out-problem-actions">
              <button type="button" onClick={() => setCode((seen) => !seen)}>
                {code ? "Hide code" : "Show code"}
              </button>
              {ask && (
                <button
                  type="button"
                  className="gold"
                  onClick={() =>
                    ask(`The ${label.toLowerCase()} "${name}" couldn't be drawn. ${problem} Please fix it.`)
                  }
                >
                  Ask Medha to fix it
                </button>
              )}
            </div>
            {code && <pre className="out-source">{source}</pre>}
          </div>
        )}
      </div>
      <figcaption>
        {reveal ? (
          <button type="button" className="out-name" onClick={reveal}>
            {name}
          </button>
        ) : (
          <span className="out-name">{name}</span>
        )}
        <Caption output={output} detail={detail} />
        <span className="out-end">
          {writing ? (
            <span className="out-status">
              <i />
              Writing
            </span>
          ) : (
            chip
          )}
          {reveal && <Icon name="external" className="icon out-opens" />}
        </span>
      </figcaption>
    </figure>
  );
});

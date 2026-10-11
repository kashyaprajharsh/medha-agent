import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Icon } from "./Icon";
import { useWorkspaceApi } from "./Workspace";
import type { ToolScreen } from "./api";
import type { Step, WritingCall } from "./live";
import { useOffered } from "./OutputApp";
import { labelOf, noteOf, screenOutput, titleOf, versionsOf, type Output } from "./outputBlocks";
import { useOutputs, type Detail } from "./outputContext";
import { useKeyed } from "./outputFiles";
import { Rendered } from "./OutputRendered";

type Props = Omit<Output, "name"> & {
  writing: boolean;
  order: number;
  path?: string;
  name?: string;
  screen?: ToolScreen;
  fresh?: boolean;
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
  const { made, all, resolve, show } = useOutputs();
  const offered = useOffered(writing);
  // A completed screen belongs to its durable event. Providers can reuse call
  // IDs, so those IDs identify only an unfinished call in this stretch of work.
  const shown = new Map<string, { screen: ToolScreen; written: boolean }>();
  for (const call of writing ?? []) {
    const tool = offered(call.tool);
    if (tool) shown.set(call.id, { screen: { ...tool, partial: call.text }, written: false });
  }
  const failed: Step[] = [];
  for (const step of steps) if (step.screen) {
    if (step.status === "ok" && !(step.screen.result as { isError?: boolean } | undefined)?.isError) {
      shown.delete(step.id);
      shown.set(step.screen.source_event ?? step.id, { screen: step.screen, written: true });
    }
    else if (step.status !== "running") { shown.delete(step.id); failed.push(step); }
  }
  const files = all.filter((output) => steps.some((step) =>
    step.sourceEvent && step.sourceEvent === output.sourceEvent && step.status === "ok" &&
    // A presentation snapshot carries the observation's event ID. Its screen
    // can be recovered from the catalogue without duplicating screen bodies in
    // every shared snapshot; the live notification already supplies its plate.
    (output.kind !== "app" || !step.screen)));
  // A screen that arrives while the person watches opens beside the chat, like anything Medha makes.
  const newest = [...shown].filter(([, entry]) => entry.written).at(-1);
  const anchor = newest && `screen:${newest[0]}`;
  const arrived = newest?.[1].screen;
  const opened = useRef<string>(undefined);
  useEffect(() => {
    if (fresh && arrived && anchor && opened.current !== anchor) { opened.current = anchor; const output = screenOutput(anchor, arrived); made?.(resolve?.(output) ?? output); }
  }, [fresh, made, anchor, arrived, resolve]);
  if (!shown.size && !failed.length && !files.length) return null;
  return (
    <>
      {failed.map((step) => <details key={step.sourceEvent ?? step.id} className="out-failed"><summary>{labelOf({ kind: "app", screen: step.screen })} couldn't finish</summary><p>{step.summary ?? step.detail ?? "The tool did not produce an output."}</p>{step.output && <pre>{step.output}</pre>}<button type="button" onClick={() => show?.(screenOutput(`error-screen:${step.sourceEvent ?? step.id}`, step.screen!))}>Inspect error screen</button></details>)}
    <div className="out-grid">
      {files.map((output, index) => <Plate key={output.anchor} {...output} writing={false} fresh={fresh} order={at * 1000 + index} />)}
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
    </>
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
  const [failure, setFailure] = useState<string>();
  const act = (call: Promise<unknown>) => { setFailure(undefined); void call.catch((error) => setFailure(String(error))); };
  return (
    <div className="out-problem">
      <b>{path.split(/[\\/]/).at(-1)}</b>
      <span>{problem}</span>
      <div className="out-problem-actions">
        <button type="button" onClick={() => act(api.revealFile(path))}>
          Show in folder
        </button>
        <button type="button" onClick={() => act(api.openFile(path))}>
          Open
        </button>
      </div>
      {failure && <span role="alert">{failure}</span>}
    </div>
  );
}

/** One thing Medha made, shown where it was made: the content, then one caption row. */
export const Plate = memo(function Plate({ anchor: requestedAnchor, kind, source, writing, order, path, name: given, screen, fresh, ...identity }: Props) {
  const { open, all, viewing, show, follow, enter, made, fix, running, resolve } = useOutputs();
  const [problem, setProblem] = useKeyed<string>(`${requestedAnchor}\0${source}\0${path}\0${screen?.resource}`);
  const [detail, setDetail] = useKeyed<Detail>(`${requestedAnchor}\0${source}\0${path}\0${screen?.resource}`);
  const [code, setCode] = useState(false);
  const [asked, setAsked] = useState(false);
  const [valid, setValid] = useState(false);
  const report = useCallback((problem: string | undefined) => { setProblem(problem); setValid(!problem); }, [setProblem]);
  const stage = useRef<HTMLDivElement>(null);
  const output = useMemo<Output>(
    () => { const output = { ...identity, anchor: requestedAnchor, kind, source, path, screen, name: given ?? titleOf(kind, source) }; return !writing && resolve ? resolve(output) : output; },
    [requestedAnchor, kind, source, path, screen, given, writing, resolve, identity.artifactId, identity.sourceEvent, identity.sourceIndex, identity.sourceDigest, identity.revisionId, identity.snapshot, identity.sessionId, identity.toolCall, identity.storedSource, identity.predecessor],
  );
  const { name, anchor } = output;
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
    }
    wrote.current = writing;
  }, [writing, made, output]);
  const opened = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!writing && valid && (fresh || developed) && opened.current !== anchor) { opened.current = anchor; made?.(output); }
  }, [writing, valid, fresh, developed, anchor, made, output]);

  const versions = versionsOf(all, output);
  const version = versions.findIndex((other) => other.anchor === anchor) + 1;
  // The same thing shown again elsewhere in the chat is current too; a screen is only ever itself.
  const current =
    here || (!!open && !!output.revisionId && open.revisionId === output.revisionId);
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
          <Rendered output={output} size="thumb" onProblem={report} />
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
            onProblem={report}
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
              {fix && (
                <button
                  type="button"
                  className="gold"
                  // One request per broken output; while Medha works, another would only steer it.
                  disabled={running || asked}
                  onClick={() => {
                    setAsked(true);
                    fix(output, `The ${label.toLowerCase()} "${name}" couldn't be drawn. ${problem} Please fix it.`);
                  }}
                >
                  {asked && running ? "Fixing…" : "Fix it"}
                </button>
              )}
              <button type="button" onClick={() => setCode((seen) => !seen)}>
                {code ? "Hide code" : "Show code"}
              </button>
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

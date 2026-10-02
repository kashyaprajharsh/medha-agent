import { useState } from "react";
import { Icon } from "./Icon";
import { latestOf, versionsOf, type Output } from "./outputBlocks";
import { useOutputs, type Detail } from "./outputContext";
import { Caption, Unshown } from "./OutputPlate";
import { Rendered } from "./OutputRendered";

function Tile({ output }: { output: Output }) {
  const { all, show } = useOutputs();
  const [problem, setProblem] = useState<string>();
  const [detail, setDetail] = useState<Detail>();
  const versions = versionsOf(all, output).length;
  return (
    <figure className={`out-plate out-tile kind-${output.kind}`}>
      <div className="out-stage opens" onClick={() => show?.(output)}>
        {!problem ? (
          <Rendered output={output} size="thumb" onProblem={setProblem} onDetail={setDetail} />
        ) : output.path ? (
          <Unshown output={output} problem={problem} />
        ) : (
          <div className="out-problem">
            <Icon name="alert" />
            <span>{problem}</span>
          </div>
        )}
      </div>
      <figcaption>
        <button type="button" className="out-name" onClick={() => show?.(output)}>
          {output.name}
        </button>
        <Caption output={output} detail={detail} />
        {versions > 1 && <span className="out-version">v{versions}</span>}
      </figcaption>
    </figure>
  );
}

function Group({ title, outputs }: { title: string; outputs: Output[] }) {
  if (!outputs.length) return null;
  return (
    <section>
      <h3>
        {title} <span>{outputs.length}</span>
      </h3>
      <div className="out-grid">
        {outputs.map((output) => (
          <Tile key={output.anchor} output={output} />
        ))}
      </div>
    </section>
  );
}

/** Everything made in this chat, newest first, each at its newest version. */
export function Gallery() {
  const { all } = useOutputs();
  const outputs = latestOf(all);
  if (!outputs.length)
    return (
      <div className="surface-body">
        <div className="surface-empty">
          <Icon name="maximize" />
          <b>Nothing made yet</b>
          <p>Diagrams, pages, images and files Medha makes in this chat open here at full size.</p>
        </div>
      </div>
    );
  // An anchor is the reply's own name, then the output's place in it.
  const turn = (output: Output) => output.anchor.slice(0, output.anchor.lastIndexOf(":"));
  const newest = turn(all.at(-1)!);
  return (
    <div className="out-gallery">
      <header>
        <b>All outputs</b>
        <span>{outputs.length} in this chat</span>
      </header>
      <Group title="This turn" outputs={outputs.filter((output) => turn(output) === newest)} />
      <Group title="Earlier in this chat" outputs={outputs.filter((output) => turn(output) !== newest)} />
    </div>
  );
}

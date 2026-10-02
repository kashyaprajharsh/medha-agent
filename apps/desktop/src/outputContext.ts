import { createContext, useContext, type MutableRefObject } from "react";
import type { Output } from "./outputBlocks";

export type Outputs = {
  open: Output | null;
  /** Every output on screen in this chat, oldest first. */
  all: Output[];
  /** The viewer is showing, so a revision need not repeat itself in the chat. */
  viewing: boolean;
  /** Pages the person has stopped, by anchor. A page runs unless it is here. */
  stopped: string[];
  /** Pages the person has allowed the web, by anchor. Nothing else ever adds to this. */
  online: string[];
  /** Absent where an output is only shown, such as inside a file preview. */
  show?: (output: Output, from?: DOMRect) => void;
  follow?: (output: Output) => void;
  enter?: (output: Output, order: number) => () => void;
  made?: (output: Output) => void;
  ask?: (text: string) => void;
  stop?: (output: Output, stopped: boolean) => void;
  allow?: (output: Output, online: boolean) => void;
  browse?: () => void;
  /** Where the open output sat in the chat, for the viewer to grow from once. */
  from?: MutableRefObject<DOMRect | null>;
};

export const SHOWN_ONLY: Outputs = { open: null, all: [], viewing: false, stopped: [], online: [] };

const Context = createContext<Outputs>(SHOWN_ONLY);

export const OutputsProvider = Context.Provider;
export const useOutputs = () => useContext(Context);

/** What a renderer learned about its output: a word for the caption, a line for the viewer. */
export type Detail = { caption?: string; foot?: string };

export type RenderProps = {
  output: Output;
  size: "plate" | "thumb" | "full";
  writing?: boolean;
  onProblem?: (problem: string | undefined) => void;
  onDetail?: (detail: Detail) => void;
  zoom?: number;
  slide?: number;
  onSlide?: (index: number) => void;
};

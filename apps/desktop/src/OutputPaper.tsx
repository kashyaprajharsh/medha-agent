import { lazy, useEffect, useRef, useState } from "react";
import { getDocument, GlobalWorkerOptions, type PDFDocumentProxy } from "pdfjs-dist";
import worker from "pdfjs-dist/build/pdf.worker.min.mjs?url";
import Papa from "papaparse";
import type { FilePreview } from "./api";
import DocumentPreview from "./DocumentPreview";
import { sizeText } from "./outputBlocks";
import type { RenderProps } from "./outputContext";
import { useFile, useNear, useWidth } from "./outputFiles";

GlobalWorkerOptions.workerSrc = worker;
const TablePreview = lazy(() => import("./TablePreview"));

const MAX_PAGES = 200;
const PLATE_ROWS = 5;

function PdfPage({ pdf, number, width }: { pdf: PDFDocumentProxy; number: number; width: number }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const near = useNear(canvas);
  useEffect(() => {
    if (!near || !width) return;
    let current = true;
    let drawing: { cancel: () => void; promise: Promise<unknown> } | undefined;
    void pdf
      .getPage(number)
      .then((page) => {
        const element = canvas.current;
        if (!current || !element) return;
        const viewport = page.getViewport({ scale: width / page.getViewport({ scale: 1 }).width });
        const ratio = Math.min(window.devicePixelRatio || 1, 2);
        element.width = Math.ceil(viewport.width * ratio);
        element.height = Math.ceil(viewport.height * ratio);
        element.style.aspectRatio = `${viewport.width} / ${viewport.height}`;
        drawing = page.render({
          canvas: element,
          canvasContext: element.getContext("2d")!,
          viewport,
          transform: [ratio, 0, 0, ratio, 0, 0],
        });
        return drawing.promise;
      })
      .catch(() => {});
    return () => {
      current = false;
      drawing?.cancel();
    };
  }, [pdf, number, width, near]);
  return <canvas ref={canvas} data-page={number} aria-label={`Page ${number}`} />;
}

function Pdf({ file, size, onProblem, onDetail }: RenderProps & { file: FilePreview }) {
  const [pdf, setPdf] = useState<PDFDocumentProxy>();
  const [box, width] = useWidth();
  const [page, setPage] = useState(1);
  useEffect(() => {
    const loading = getDocument({ data: new Uint8Array(file.bytes!), useSystemFonts: true });
    let current = true;
    loading.promise
      .then((opened) => current && setPdf(opened))
      .catch(() => current && onProblem?.("This PDF could not be read."));
    return () => {
      current = false;
      void loading.destroy();
    };
  }, [file, onProblem]);
  const pages = pdf?.numPages ?? 0;
  useEffect(() => {
    if (!pages) return;
    onDetail?.({
      caption: `${pages} ${pages === 1 ? "page" : "pages"}`,
      foot: `Page ${page} of ${pages}, ${sizeText(file.size)}.`,
    });
  }, [onDetail, pages, page, file.size]);
  // The page most on screen is the one the viewer names below.
  useEffect(() => {
    const scroller = box.current?.closest(".out-canvas");
    if (size !== "full" || !scroller || !pages) return;
    const read = () => {
      const frame = scroller.getBoundingClientRect();
      const middle = frame.top + frame.height / 2;
      const sheets = [...scroller.querySelectorAll("canvas")];
      const at = sheets.findIndex((sheet) => sheet.getBoundingClientRect().bottom > middle);
      setPage(at < 0 ? sheets.length : at + 1);
    };
    // Pages take their height as they are drawn, so the reading follows that too.
    const sized = new ResizeObserver(read);
    sized.observe(box.current!);
    scroller.addEventListener("scroll", read, { passive: true });
    return () => {
      sized.disconnect();
      scroller.removeEventListener("scroll", read);
    };
  }, [box, size, pages]);
  const shown = size === "full" ? Math.min(pages, MAX_PAGES) : Math.min(pages, 1);
  return (
    <div className="out-pdf" ref={box}>
      {pdf &&
        Array.from({ length: shown }, (_, index) => (
          <PdfPage key={index} pdf={pdf} number={index + 1} width={width} />
        ))}
    </div>
  );
}

function Table({ file, size, onDetail }: RenderProps & { file: FilePreview }) {
  const [rows, setRows] = useState<unknown[][]>();
  useEffect(() => {
    let current = true;
    if (file.kind === "table") {
      const delimiter = file.extension === "tsv" ? "\t" : "";
      setRows(Papa.parse<string[]>(file.text ?? "", { delimiter, skipEmptyLines: true }).data);
    } else
      void import("read-excel-file/browser")
        .then((module) => module.default(new Blob([new Uint8Array(file.bytes!)])))
        .then((sheets) => current && setRows(sheets[0]?.data ?? []))
        .catch(() => current && setRows([]));
    return () => {
      current = false;
    };
  }, [file]);
  const count = rows ? Math.max(0, rows.length - 1) : undefined;
  useEffect(() => {
    if (count !== undefined) onDetail?.({ caption: `${count} ${count === 1 ? "row" : "rows"}` });
  }, [onDetail, count]);
  if (size === "full") return <TablePreview file={file} />;
  if (!rows) return <div className="out-wait" />;
  const [head = [], ...body] = rows;
  const cell = (value: unknown) => (value instanceof Date ? value.toLocaleDateString() : String(value ?? ""));
  return (
    <table className="out-table">
      <thead>
        <tr>
          {head.slice(0, 6).map((value, column) => (
            <th key={column}>{cell(value)}</th>
          ))}
        </tr>
      </thead>
      <tbody>
        {body.slice(0, PLATE_ROWS).map((row, index) => (
          <tr key={index}>
            {head.slice(0, 6).map((_, column) => (
              <td key={column}>{cell(row[column])}</td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** A document, a PDF or a table Medha wrote to a file, read and shown on paper. */
export default function Paper(props: RenderProps) {
  const file = useFile(props.output.path, props.onProblem);
  if (!file) return <div className="out-wait" />;
  if (props.output.kind === "pdf") return <Pdf {...props} file={file} />;
  if (props.output.kind === "table") return <Table {...props} file={file} />;
  return (
    <div className="out-document">
      <DocumentPreview bytes={file.bytes!} />
    </div>
  );
}

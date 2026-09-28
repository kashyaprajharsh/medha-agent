import { Select } from "./Select";
import { useEffect, useRef, useState } from "react";
import {
  getDocument,
  GlobalWorkerOptions,
  type PDFDocumentProxy,
} from "pdfjs-dist";
import worker from "pdfjs-dist/build/pdf.worker.min.mjs?url";
GlobalWorkerOptions.workerSrc = worker;
export default function PdfPreview({ bytes }: { bytes: number[] }) {
  const [pdf, setPdf] = useState<PDFDocumentProxy>();
  const [page, setPage] = useState(1);
  const [zoom, setZoom] = useState(1);
  const [error, setError] = useState("");
  const canvas = useRef<HTMLCanvasElement>(null);
  const container = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(400);
  useEffect(() => {
    const box = container.current;
    if (!box) return;
    const observer = new ResizeObserver(() =>
      setWidth(Math.max(240, box.clientWidth - 24)),
    );
    observer.observe(box);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    let active = true;
    const loading = getDocument({
      data: new Uint8Array(bytes),
      useSystemFonts: true,
    });
    loading.promise
      .then((document) => {
        if (active) setPdf(document);
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
      void loading.destroy();
    };
  }, [bytes]);
  useEffect(() => {
    if (!pdf || !canvas.current) return;
    let active = true;
    let render:
      | ReturnType<Awaited<ReturnType<PDFDocumentProxy["getPage"]>>["render"]>
      | undefined;
    void pdf
      .getPage(page)
      .then((document) => {
        if (!active || !canvas.current) return;
        const natural = document.getViewport({ scale: 1 });
        const viewport = document.getViewport({
          scale: (width / natural.width) * zoom,
        });
        const ratio = Math.min(window.devicePixelRatio || 1, 2);
        const element = canvas.current;
        element.width = Math.ceil(viewport.width * ratio);
        element.height = Math.ceil(viewport.height * ratio);
        element.style.width = `${viewport.width}px`;
        element.style.height = `${viewport.height}px`;
        render = document.render({
          canvas: element,
          canvasContext: element.getContext("2d")!,
          viewport,
          transform: [ratio, 0, 0, ratio, 0, 0],
        });
        return render.promise;
      })
      .catch((cause) => {
        if (active && cause?.name !== "RenderingCancelledException")
          setError(String(cause));
      });
    return () => {
      active = false;
      render?.cancel();
    };
  }, [pdf, page, zoom, width]);
  return (
    <div className="pdf-preview" ref={container}>
      <div className="preview-controls">
        <button
          className="btn-line"
          disabled={!pdf || page <= 1}
          onClick={() => setPage(page - 1)}
        >
          Previous
        </button>
        <span>
          Page {page} of {pdf?.numPages ?? "…"}
        </span>
        <button
          className="btn-line"
          disabled={!pdf || page >= pdf.numPages}
          onClick={() => setPage(page + 1)}
        >
          Next
        </button>
        <Select
          aria-label="PDF zoom"
          value={zoom}
          onChange={(event) => setZoom(Number(event.target.value))}
        >
          {[0.75, 1, 1.25, 1.5, 2].map((value) => (
            <option key={value} value={value}>
              {Math.round(value * 100)}%
            </option>
          ))}
        </Select>
      </div>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      <div className="pdf-canvas">
        <canvas ref={canvas} aria-label={`PDF page ${page}`} />
      </div>
    </div>
  );
}

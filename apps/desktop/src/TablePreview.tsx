import { Select } from "./Select";
import { useEffect, useState } from "react";
import type { FilePreview } from "./api";
import Papa from "papaparse";
type Sheet = { name: string; rows: unknown[][] };
export default function TablePreview({ file }: { file: FilePreview }) {
  const [sheets, setSheets] = useState<Sheet[]>([]);
  const [selected, setSelected] = useState(0);
  const [page, setPage] = useState(0);
  const [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    setSheets([]);
    setError("");
    setPage(0);
    setSelected(0);
    if (file.kind === "table") {
      const parsed = Papa.parse<string[]>(file.text || "", {
        delimiter: file.extension === "tsv" ? "\t" : "",
        skipEmptyLines: true,
      });
      setSheets([{ name: "Table", rows: parsed.data }]);
      if (parsed.errors.length) setError(parsed.errors[0].message);
    } else {
      void import("read-excel-file/browser")
        .then((module) =>
          module.default(new Blob([new Uint8Array(file.bytes!)])),
        )
        .then((rows) => {
          if (active)
            setSheets(
              rows.map((sheet) => ({ name: sheet.sheet, rows: sheet.data })),
            );
        })
        .catch((cause) => active && setError(String(cause)));
    }
    return () => {
      active = false;
    };
  }, [file]);
  const sheet = sheets[selected];
  const rows = sheet?.rows.slice(page * 100, (page + 1) * 100) || [];
  const columns = Math.min(100, Math.max(0, ...rows.map((row) => row.length)));
  return (
    <>
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      <div className="preview-controls">
        {sheets.length > 1 && (
          <Select
            aria-label="Worksheet"
            value={selected}
            onChange={(event) => {
              setSelected(Number(event.target.value));
              setPage(0);
            }}
          >
            {sheets.map((item, index) => (
              <option key={index} value={index}>
                {item.name}
              </option>
            ))}
          </Select>
        )}
        <span>{sheet?.rows.length ?? 0} rows</span>
        <button
          className="btn-line"
          disabled={!page}
          onClick={() => setPage(page - 1)}
        >
          Previous
        </button>
        <button
          className="btn-line"
          disabled={!sheet || (page + 1) * 100 >= sheet.rows.length}
          onClick={() => setPage(page + 1)}
        >
          Next
        </button>
      </div>
      <div className="sheet-scroll">
        <table className="sheet-table">
          <thead>
            <tr>
              <th>#</th>
              {Array.from({ length: columns }, (_, column) => (
                <th key={column}>{columnName(column)}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, index) => (
              <tr key={index}>
                <th>{page * 100 + index + 1}</th>
                {Array.from({ length: columns }, (_, column) => (
                  <td key={column}>
                    {row[column] instanceof Date
                      ? (row[column] as Date).toLocaleDateString()
                      : String(row[column] ?? "")}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {columns === 100 && (
        <p className="quiet">Showing the first 100 columns.</p>
      )}
    </>
  );
}
function columnName(index: number) {
  let name = "";
  for (let value = index + 1; value; value = Math.floor((value - 1) / 26))
    name = String.fromCharCode(65 + ((value - 1) % 26)) + name;
  return name;
}

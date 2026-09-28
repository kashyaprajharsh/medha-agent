import { useEffect, useState } from "react";
import DOMPurify from "dompurify";
export default function DocumentPreview({ bytes }: { bytes: number[] }) {
  const [html, setHtml] = useState("");
  const [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    void import("mammoth")
      .then((mammoth) =>
        mammoth.convertToHtml({ arrayBuffer: new Uint8Array(bytes).buffer }),
      )
      .then((result) => {
        if (active)
          setHtml(
            DOMPurify.sanitize(result.value, {
              ALLOWED_TAGS: [
                "p",
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
                "strong",
                "em",
                "u",
                "s",
                "ul",
                "ol",
                "li",
                "table",
                "thead",
                "tbody",
                "tr",
                "td",
                "th",
                "br",
                "sup",
                "sub",
                "blockquote",
                "img",
              ],
              ALLOWED_ATTR: ["src", "alt", "colspan", "rowspan"],
              ADD_URI_SAFE_ATTR: [],
              FORBID_ATTR: ["style"],
            }).replace(/src="(?!data:image\/)[^"]*"/g, ""),
          );
      })
      .catch((cause) => active && setError(String(cause)));
    return () => {
      active = false;
    };
  }, [bytes]);
  return (
    <>
      {error ? (
        <p role="alert" className="surface-error">
          {error}
        </p>
      ) : (
        <>
          <p className="quiet">Reading view</p>
          <article
            className="document-preview md"
            dangerouslySetInnerHTML={{ __html: html }}
          />
        </>
      )}
    </>
  );
}

import { useEffect, useState } from "react";
import { Icon } from "./Icon";
import { Sheet } from "./Sheet";
import { useWorkspaceApi } from "./Workspace";

type File = { path: string; text: string };
/** The backend's own reading of SKILL.md, the same one an install uses. */
type Check = { name?: string; description?: string; required_tools?: string[]; error?: string };

const START = `---
name:
description:
---

# Steps

1.
`;
// Below this a description rarely says both what the skill does and when to use it.
const SHORT = 40;
const LIMIT = 1024;

type Props = {
  disabled: boolean;
  /** A draft to start from, such as one made in a chat. */
  initial?: File[];
  onClose: () => void;
  onSaved: (name: string, enabled: boolean, findings: string[]) => void;
};

/** Writing a skill by hand: SKILL.md and its files, checked as they are typed, saved through the install scan. */
export function SkillWriter({ disabled, initial, onClose, onSaved }: Props) {
  const api = useWorkspaceApi();
  const [files, setFiles] = useState<File[]>(initial ?? [{ path: "SKILL.md", text: START }]);
  const [open, setOpen] = useState(0);
  const [check, setCheck] = useState<Check>({});
  const [scope, setScope] = useState<"user" | "project">("user");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [taken, setTaken] = useState(false);
  const skillMd = files[0].text;

  useEffect(() => {
    let current = true;
    const wait = setTimeout(() => {
      void api
        .extensions("extensions.skill.check", { text: skillMd })
        .then((seen) => current && setCheck(seen as Check))
        .catch((cause) => current && setCheck({ error: String(cause) }));
    }, 250);
    return () => {
      current = false;
      clearTimeout(wait);
    };
  }, [skillMd]);

  async function save(replace = false) {
    setSaving(true);
    setError("");
    setTaken(false);
    try {
      const saved = await api.extensions("extensions.skill.write", { files, scope, replace });
      onSaved(String(saved.name), Boolean(saved.enabled), (saved.findings as string[]) ?? []);
      onClose();
    } catch (cause) {
      const said = String(cause);
      setTaken(/already exists/.test(said));
      setError(said);
    } finally {
      setSaving(false);
    }
  }
  const edit = (text: string) => {
    if (open === 0) setTaken(false);
    setFiles(files.map((file, index) => (index === open ? { ...file, text } : file)));
  };
  const choose = (where: "user" | "project") => {
    setTaken(false);
    setScope(where);
  };
  const rename = (path: string) => setFiles(files.map((file, index) => (index === open ? { ...file, path } : file)));
  const add = () => {
    let path = "references/notes.md";
    for (let n = 2; files.some((file) => file.path === path); n++) path = `references/notes-${n}.md`;
    setFiles([...files, { path, text: "" }]);
    setOpen(files.length);
  };
  const remove = () => {
    setFiles(files.filter((_, index) => index !== open));
    setOpen(0);
  };
  const length = check.description?.length ?? 0;
  const ready = !check.error && Boolean(check.name);

  return (
    <Sheet label="Write a skill" onClose={onClose} wide>
      <div className="sheet-title">
        <h2>Write a skill</h2>
      </div>
      <div className="writer">
        <nav className="writer-files" aria-label="Files">
          {files.map((file, index) => (
            <button key={index} className={index === open ? "on" : undefined} onClick={() => setOpen(index)}>
              {file.path || "untitled"}
            </button>
          ))}
          <button className="writer-add" onClick={add}>
            <Icon name="plus" /> Add file
          </button>
        </nav>
        <div className="writer-body">
          {open > 0 && (
            <div className="writer-path">
              <input
                aria-label="File path"
                value={files[open].path}
                spellCheck={false}
                onChange={(event) => rename(event.target.value)}
              />
              <button className="sheet-quiet" onClick={remove}>
                Remove file
              </button>
            </div>
          )}
          <textarea
            className="writer-code"
            aria-label={files[open].path || "File text"}
            spellCheck={false}
            value={files[open].text}
            onChange={(event) => edit(event.target.value)}
          />
        </div>
      </div>
      <ul className="writer-checks" aria-label="Checks">
        {skillMd === START ? (
          <>
            <li className="quiet">Name it in short kebab-case, like release-notes</li>
            <li className="quiet">Describe what it does and when to use it</li>
          </>
        ) : check.error ? (
          <li className="bad">{check.error}</li>
        ) : check.name ? (
          <>
            <li>Name {check.name} is short kebab-case</li>
            <li className={length < SHORT ? "warn" : undefined}>
              {length < SHORT
                ? `Description is short: say what it does and when to use it`
                : `Description is ${length} of ${LIMIT} characters`}
            </li>
            {check.required_tools?.length ? <li>Needs {check.required_tools.join(", ")}</li> : null}
          </>
        ) : (
          <li className="quiet">Checking</li>
        )}
      </ul>
      {ready && (
        <>
          <h3 className="sheet-h">How Medha will see it</h3>
          <p className="intake-listing">
            <b>{check.name}</b>
            <span>{check.description}</span>
          </p>
        </>
      )}
      {error && (
        <p className="surface-error" role="alert">
          {error}
        </p>
      )}
      <div className="sheet-actions writer-actions">
        <label className="intake-choice">
          <input type="radio" name="writer-for" checked={scope === "user"} onChange={() => choose("user")} /> Me, every
          project
        </label>
        <label className="intake-choice">
          <input type="radio" name="writer-for" checked={scope === "project"} onChange={() => choose("project")} />{" "}
          This project only
        </label>
        <span className="writer-spacer" />
        <button className="btn-line" onClick={onClose}>
          Cancel
        </button>
        {taken ? (
          <button className="btn-gold" disabled={disabled || saving} onClick={() => void save(true)}>
            Replace it
          </button>
        ) : (
          <button className="btn-gold" disabled={disabled || saving || !ready} onClick={() => void save()}>
            Save skill
          </button>
        )}
      </div>
    </Sheet>
  );
}

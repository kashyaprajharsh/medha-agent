import { useState } from "react";
import { restOf, summaryOf } from "./ExtensionParts";
import { Sheet } from "./Sheet";

export type Skill = {
  name: string;
  description: string;
  scope: string;
  enabled: boolean;
  available: boolean | null;
  missing_tools: string[];
};

const GROUPS = [
  ["user", "Yours", "used in every project", "Every project on this computer"],
  ["project", "This project", "travels with the repository", "This project only"],
  ["plugin", "From plugins", "on and off with their plugin", "Wherever its plugin is on"],
] as const;

// How much of a description the sheet shows before offering the rest.
const LONG = 320;

type Props = {
  skills: Skill[];
  /** Whether a skill passes the list's search. */
  matches: (name: string, description: string) => boolean;
  searching: boolean;
  disabled: boolean;
  onToggle: (skill: Skill) => void;
  onRemove: (skill: Skill) => void;
  onAsk: (prompt: string) => void;
};

/** The installed skills, one line each; a row opens the skill in full. */
export function SkillList({ skills, matches, searching, disabled, onToggle, onRemove, onAsk }: Props) {
  const [openName, setOpenName] = useState("");
  const [whole, setWhole] = useState(false);
  const open = skills.find((skill) => skill.name === openName);
  const needs = (skill: Skill) => skill.available === false && skill.enabled;
  const control = (skill: Skill) =>
    skill.scope !== "plugin" ? (
      <button
        className="switch quiet-on"
        role="switch"
        aria-checked={skill.enabled}
        aria-label={`Use ${skill.name}`}
        disabled={disabled}
        onClick={() => onToggle(skill)}
      />
    ) : (
      <small className="ix-note">Managed by plugin</small>
    );
  return (
    <>
      {GROUPS.map(([scope, title, note]) => {
        const rows = skills.filter((skill) => skill.scope === scope && matches(skill.name, skill.description));
        if (!rows.length)
          // The project's own shelf is named even while it is empty, so it can be found.
          return scope === "project" && !searching && skills.length > 0 ? (
            <section className="ix-group" key={scope}>
              <h3>
                {title} <span>nothing yet</span>
              </h3>
              <p className="ix-hint">
                Skills in this project's <code>.medha/skills</code> folder show here and travel with the repository.
              </p>
            </section>
          ) : null;
        return (
          <section className="ix-group" key={scope}>
            <h3>
              {title} <span>{note}</span>
            </h3>
            <div className="ix">
              {rows.map((skill) => (
                <div className={`ix-row ${skill.enabled ? "" : "off"}`} key={skill.name}>
                  <button
                    className="ix-main"
                    onClick={() => {
                      setWhole(false);
                      setOpenName(skill.name);
                    }}
                  >
                    <b>{skill.name}</b>
                    <span className="ix-sum">{summaryOf(skill.description)}</span>
                  </button>
                  {needs(skill) && <span className="ix-chip warn">Needs {skill.missing_tools.join(", ")}</span>}
                  {control(skill)}
                </div>
              ))}
            </div>
          </section>
        );
      })}
      {!skills.length && (
        <p className="quiet">
          No skills installed. Add SKILL.md packages in your Medha skills folder, or install a plugin that includes
          skills.
        </p>
      )}
      {open && (
        <Sheet label={open.name} onClose={() => setOpenName("")}>
          <div className="sheet-title">
            <h2>{open.name}</h2>
            {control(open)}
          </div>
          <p className="sheet-lead">{summaryOf(open.description)}</p>
          {restOf(open.description) && (
            <p className={`sheet-more ${whole ? "" : "clamp"}`}>
              {restOf(open.description)}
              {restOf(open.description).length > LONG && (
                <button onClick={() => setWhole(!whole)}>{whole ? "Show less" : "Show all"}</button>
              )}
            </p>
          )}
          <dl className="sheet-facts">
            <dt>Used in</dt>
            <dd>{GROUPS.find(([scope]) => scope === open.scope)?.[3]}</dd>
            <dt>Loads</dt>
            <dd>
              When your task matches, or when you pick it from the <kbd>/</kbd> menu
            </dd>
            <dt>Can touch</dt>
            <dd>Nothing by itself. It is instructions; Medha still asks before each action.</dd>
            {needs(open) && (
              <>
                <dt>Needs</dt>
                <dd className="warn">{open.missing_tools.join(", ")}, which this chat does not have</dd>
              </>
            )}
          </dl>
          <div className="sheet-actions">
            <button
              className="btn-gold"
              disabled={!open.enabled}
              onClick={() => {
                setOpenName("");
                onAsk(`Use the ${open.name} skill to `);
              }}
            >
              Try it in a new chat
            </button>
            {open.scope === "user" && (
              <button
                className="sheet-quiet"
                disabled={disabled}
                onClick={() => {
                  // The question that follows is asked on the page, not over this sheet.
                  setOpenName("");
                  onRemove(open);
                }}
              >
                Remove
              </button>
            )}
          </div>
        </Sheet>
      )}
    </>
  );
}

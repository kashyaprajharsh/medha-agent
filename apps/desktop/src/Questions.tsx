import { useState } from "react";
import { Icon } from "./Icon";
import type { QuestionForm } from "./live";

export function Questions({
  form,
  onAnswer,
}: {
  form: QuestionForm;
  onAnswer: (answers?: { selected: string[]; other?: string }[]) => void;
}) {
  const [drafts, setDrafts] = useState(() =>
    form.questions.map(() => ({ selected: [] as string[], other: "" })),
  );
  const complete = drafts.every(
    (draft) => draft.selected.length > 0 || draft.other.trim().length > 0,
  );
  return (
    <section className="question-card" aria-label="Medha needs your input">
      <div className="ask-top">
        <Icon name="chat" />
        <b>A decision before continuing</b>
      </div>
      {form.questions.map((question, index) => (
        <fieldset key={index} disabled={form.pending}>
          <legend>
            {question.header && <small>{question.header}</small>}
            {question.prompt}
          </legend>
          <div className="question-options">
            {question.options.map((option) => (
              <label
                key={option.label}
                className={
                  drafts[index].selected.includes(option.label)
                    ? "selected"
                    : ""
                }
              >
                <input
                  type={question.multi_select ? "checkbox" : "radio"}
                  name={`question-${form.id}-${index}`}
                  checked={drafts[index].selected.includes(option.label)}
                  onChange={() =>
                    setDrafts((all) =>
                      all.map((draft, position) =>
                        position !== index
                          ? draft
                          : {
                              ...draft,
                              selected: question.multi_select
                                ? draft.selected.includes(option.label)
                                  ? draft.selected.filter(
                                      (label) => label !== option.label,
                                    )
                                  : [...draft.selected, option.label]
                                : [option.label],
                            },
                      ),
                    )
                  }
                />
                <span>
                  <b>{option.label}</b>
                  {option.recommended && <em>Suggested</em>}
                  {option.description && <small>{option.description}</small>}
                </span>
              </label>
            ))}
          </div>
          <input
            className="custom-answer"
            aria-label={`Your own answer to ${question.header || question.prompt}`}
            placeholder="Or write your own answer…"
            value={drafts[index].other}
            onChange={(event) =>
              setDrafts((all) =>
                all.map((draft, position) =>
                  position !== index
                    ? draft
                    : { ...draft, other: event.target.value },
                ),
              )
            }
          />
        </fieldset>
      ))}
      <div className="ask-actions">
        <button
          type="button"
          className="btn-gold"
          disabled={!complete || form.pending}
          onClick={() =>
            onAnswer(
              drafts.map((draft) => ({
                selected: draft.selected,
                ...(draft.other.trim() ? { other: draft.other.trim() } : {}),
              })),
            )
          }
        >
          {form.pending ? "Sending…" : "Continue"}
        </button>
        <button
          type="button"
          className="btn-line"
          disabled={form.pending}
          onClick={() => onAnswer()}
        >
          Skip
        </button>
      </div>
    </section>
  );
}

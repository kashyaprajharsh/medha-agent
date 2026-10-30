import { useState } from "react";
import { Icon } from "./Icon";
import { named, type Platform } from "./MessagingSetup";

export type Person = { platform: string; user: string; name: string };
export type Asking = Person & { minutes: number };

/** The projects a phone may work in: the one new chats start in, then the others. */
export function ProjectsSheet({
  folders,
  current,
  onDefault,
  onRemove,
  onAdd,
}: {
  folders: string[];
  current: string | null;
  onDefault: (folder: string) => void;
  onRemove: (folder: string) => void;
  onAdd: () => void;
}) {
  return (
    <>
      <div className="sheet-title">
        <h2>Projects</h2>
      </div>
      <p className="sheet-lead">
        Your phone can work only in these. On your phone, <kbd>/new</kbd> asks which one a new chat works in.
      </p>
      <div className="ix ms-list">
        {folders.map((folder) => {
          const starts = folder === current;
          return (
            <div className="ix-row" key={folder}>
              <span className="ix-main ms-static">
                <b>{named(folder)}</b>
                <span className="ix-sum">{folder}</span>
              </span>
              {starts ? (
                <span className="ix-chip ms-starts">New chats</span>
              ) : (
                <span className="ms-row-actions">
                  <button type="button" className="cx-link" onClick={() => onDefault(folder)}>
                    Start new chats here
                  </button>
                  <button
                    type="button"
                    className="icon-btn sm"
                    aria-label={`Remove ${named(folder)}`}
                    title="Remove"
                    onClick={() => onRemove(folder)}
                  >
                    <Icon name="x" />
                  </button>
                </span>
              )}
            </div>
          );
        })}
      </div>
      <div className="sheet-actions">
        <button type="button" className="btn-line" onClick={onAdd}>
          <Icon name="plus" />
          Add a project
        </button>
      </div>
    </>
  );
}

/** Who may talk to Medha through the bots, and who is waiting to be let in. */
export function PeopleSheet({
  platforms,
  people,
  waiting,
  onApprove,
  onRemove,
}: {
  platforms: Platform[];
  people: Person[];
  waiting: Asking[];
  onApprove: (platform: string, code: string) => Promise<boolean>;
  onRemove: (person: Person) => void;
}) {
  const [letting, setLetting] = useState<{ platform: string; user: string; code: string }>();
  const label = (name: string) => platforms.find((p) => p.name === name)?.label ?? name;
  const scanned = platforms.filter((platform) => platform.ready && platform.owner_only);
  return (
    <>
      <div className="sheet-title">
        <h2>People</h2>
      </div>
      <p className="sheet-lead">Who can talk to Medha through your bots.</p>
      {scanned.map((platform) => (
        <p key={platform.name} className="ms-note">
          <Icon name="lock" />
          <span>
            Only you can message {platform.account ?? `your ${platform.label} bot`}: it was made by scanning. To let
            others use Medha, set {platform.label} up again with a bot you made in @BotFather.
          </span>
        </p>
      ))}
      {waiting.length > 0 && (
        <>
          <h4 className="ms-group">Waiting for you to let them in</h4>
          <div className="ix ms-list">
            {waiting.map((asking) => {
              const mine = letting?.platform === asking.platform && letting.user === asking.user;
              return (
                <div className="ix-row" key={`${asking.platform}:${asking.user}`}>
                  <span className="ix-main ms-static">
                    <b>{asking.name}</b>
                    <span className="ix-sum">
                      {label(asking.platform)}, asked {asking.minutes === 0 ? "just now" : `${asking.minutes} min ago`}
                    </span>
                  </span>
                  {mine ? (
                    <form
                      className="ms-code"
                      onSubmit={async (event) => {
                        event.preventDefault();
                        if (letting.code.trim() && (await onApprove(asking.platform, letting.code.trim()))) {
                          setLetting(undefined);
                        }
                      }}
                    >
                      <input
                        autoFocus
                        value={letting.code}
                        placeholder="Their code"
                        aria-label={`Code sent to ${asking.name}`}
                        onChange={(event) => setLetting({ ...letting, code: event.target.value.toUpperCase() })}
                        onKeyDown={(event) => event.key === "Escape" && setLetting(undefined)}
                      />
                      <button type="submit" className="btn-gold" disabled={!letting.code.trim()}>
                        Let in
                      </button>
                    </form>
                  ) : (
                    <button
                      type="button"
                      className="btn-line"
                      onClick={() => setLetting({ platform: asking.platform, user: asking.user, code: "" })}
                    >
                      Let in
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        </>
      )}
      <h4 className="ms-group">Can use Medha</h4>
      {people.length ? (
        <div className="ix ms-list">
          {people.map((person) => (
            <div className="ix-row" key={`${person.platform}:${person.user}`}>
              <span className="ix-main ms-static">
                <b>{person.name}</b>
                <span className="ix-sum">
                  {label(person.platform)} {person.user}
                </span>
              </span>
              <button
                type="button"
                className="icon-btn sm"
                aria-label={`Remove ${person.name}`}
                title="Remove"
                onClick={() => onRemove(person)}
              >
                <Icon name="x" />
              </button>
            </div>
          ))}
        </div>
      ) : (
        <p className="quiet">Nobody yet. Pairing adds you.</p>
      )}
    </>
  );
}

import { useState } from "react";

export type Series = { key: string; label: string; slot: number };
export type Day = { day: string; values: Record<string, number>; cost: string };

/** Model identity colours, fixed by rank in the whole window so a filter never repaints a model. */
export const SLOTS = 4;

const LONG_DATE: Intl.DateTimeFormatOptions = { weekday: "short", month: "short", day: "numeric" };
const SHORT_DATE: Intl.DateTimeFormatOptions = { month: "short", day: "numeric" };

function date(day: string, format: Intl.DateTimeFormatOptions) {
  const [year, month, dayOfMonth] = day.split("-").map(Number);
  return new Date(year, month - 1, dayOfMonth).toLocaleDateString(undefined, format);
}

/** A clean ceiling (1, 2 or 5 times a power of ten) so the scale reads at a glance. */
function niceCeil(value: number) {
  const power = 10 ** Math.floor(Math.log10(Math.max(1, value)));
  return ([1, 2, 5, 10].find((step) => step * power >= value) ?? 10) * power;
}

/** Tokens per local day, stacked by model, with a crosshair-free per-day tooltip. */
export function UsageChart({ days, series, format }: { days: Day[]; series: Series[]; format: (count: number) => string }) {
  const [active, setActive] = useState<number>();
  const totals = days.map((day) => series.reduce((sum, item) => sum + (day.values[item.key] ?? 0), 0));
  const hasTokens = totals.some((total) => total > 0);
  const top = niceCeil(Math.max(2, ...totals));
  const every = days.length <= 7 ? 1 : days.length <= 31 ? 7 : 15;
  const labelled = new Set(days.map((_, index) => index).filter((index) => (days.length - 1 - index) % every === 0));
  const shown = active === undefined ? undefined : days[active];
  const tick = (value: number) => format(value).replace(/\.0(?=[kM]?$)/, "");

  return (
    <figure className="usage-chart">
      {series.length > 1 && (
        <ul className="usage-legend" aria-label="Models">
          {series.map((item) => (
            <li key={item.key}>
              <i className={`usage-swatch slot-${item.slot}`} aria-hidden="true" />
              {item.label}
            </li>
          ))}
        </ul>
      )}
      <div className="usage-plot">
        <div className="usage-y" aria-hidden="true">
          <span>{hasTokens ? tick(top) : ""}</span>
          <span>{hasTokens ? tick(top / 2) : ""}</span>
          <span>0</span>
        </div>
        <div
          className="usage-bars"
          role="group"
          tabIndex={0}
          aria-label="Tokens per day. Use the arrow keys to read each day."
          onKeyDown={(event) => {
            if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
            event.preventDefault();
            const step = event.key === "ArrowLeft" ? -1 : 1;
            setActive((current) => Math.min(days.length - 1, Math.max(0, (current ?? days.length) + step)));
          }}
          onBlur={() => setActive(undefined)}
          onPointerLeave={() => setActive(undefined)}
        >
          {days.map((day, index) => (
            <div
              key={day.day}
              className={`usage-col${active === index ? " active" : ""}`}
              onPointerEnter={() => setActive(index)}
            >
              <span className="usage-stack" style={{ height: `${(totals[index] / top) * 100}%` }}>
                {series.map((item) =>
                  day.values[item.key] ? (
                    <i key={item.key} className={`slot-${item.slot}`} style={{ flexGrow: day.values[item.key] }} />
                  ) : null,
                )}
              </span>
            </div>
          ))}
          {shown && active !== undefined && (
            <div
              className="usage-tip"
              role="status"
              style={{ left: `${((active + 0.5) / days.length) * 100}%` }}
              data-side={active > days.length / 2 ? "left" : "right"}
            >
              <b>{format(totals[active])} tokens</b>
              <small>{date(shown.day, LONG_DATE)}</small>
              {totals[active] > 0 ? (
                <>
                  {series.length > 1 &&
                    series
                      .filter((item) => shown.values[item.key])
                      .map((item) => (
                        <span key={item.key} className="usage-tip-row">
                          <i className={`usage-key slot-${item.slot}`} aria-hidden="true" />
                          <b>{format(shown.values[item.key])}</b> {item.label}
                        </span>
                      ))}
                  <small>Cost {shown.cost}</small>
                </>
              ) : (
                <small>No tokens recorded</small>
              )}
            </div>
          )}
        </div>
        <div className="usage-x" aria-hidden="true">
          {days.map((day, index) => (
            <span key={day.day}>{labelled.has(index) ? date(day.day, SHORT_DATE) : ""}</span>
          ))}
        </div>
      </div>
    </figure>
  );
}

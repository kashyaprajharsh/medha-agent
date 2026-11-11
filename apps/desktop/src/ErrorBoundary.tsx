import { Component, Fragment, lazy, type ErrorInfo, type ReactNode } from "react";

/** Bumped by Try again, so a part whose load failed loads again; one that loaded is kept. */
let retries = 0;

/** A part loaded on demand, like `lazy`, whose failed load is tried again after Try again. */
export function reloadable<P extends object>(load: () => Promise<{ default: (props: P) => ReactNode }>) {
  let failed = false;
  const attempt = () =>
    load().catch((error: unknown) => {
      failed = true;
      throw error;
    });
  let made = retries;
  let Part = lazy(attempt);
  return function Reloadable(props: P) {
    if (failed && made !== retries) {
      failed = false;
      made = retries;
      Part = lazy(attempt);
    }
    return <Part {...props} />;
  };
}

type Props = {
  /** What failed, as it reads after "Couldn't open". */
  what: string;
  /** The whole window: drawn solid over everything, never through glass. */
  window?: boolean;
  children: ReactNode;
};

type State = { error: Error | null; attempt: number };

/** Keeps a part that fails to load or draw in its own place, saying why, so the rest of the window stays. */
export class Boundary extends Component<Props, State> {
  state: State = { error: null, attempt: 0 };

  static getDerivedStateFromError(error: unknown): Partial<State> {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(`[boundary: ${this.props.what}]`, error, info.componentStack);
  }

  private again = () => {
    retries += 1;
    this.setState(({ attempt }) => ({ error: null, attempt: attempt + 1 }));
  };

  render() {
    const { error, attempt } = this.state;
    if (!error) return <Fragment key={attempt}>{this.props.children}</Fragment>;
    return (
      <div className={this.props.window ? "boundary boundary-window" : "boundary"} role="alert">
        <p className="boundary-title">Couldn't open {this.props.what}.</p>
        {error.message && (
          <details className="boundary-why">
            <summary>Details</summary>
            <pre>{error.message}</pre>
          </details>
        )}
        <div className="boundary-actions">
          <button type="button" className="btn-gold" onClick={this.again}>
            Try again
          </button>
          <button type="button" className="btn-line" onClick={() => window.location.reload()}>
            Reload window
          </button>
        </div>
      </div>
    );
  }
}

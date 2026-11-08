import { Component, type ReactNode } from "react";

interface Props {
  children: ReactNode;
  fallback?: ReactNode | ((error: Error) => ReactNode);
  /** When any member changes between renders, a stale caught error is cleared so children render
   * again (e.g. the data that fed the throw). */
  resetKeys?: readonly unknown[];
}

interface State {
  error: Error | null;
  resetKeys?: readonly unknown[];
}

/** Contains a render-time throw from server-controlled content (chart configs, dynamic renderers)
 * so one bad widget does not unmount the app. */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  static getDerivedStateFromProps(props: Props, state: State): Partial<State> | null {
    const prev = state.resetKeys;
    const next = props.resetKeys;
    // Compare by value: callers pass a fresh array each render, so identity would clear the error
    // immediately.
    const changed = prev?.length !== next?.length || next?.some((key, i) => key !== prev?.[i]);
    if (!changed) return null;
    return state.error ? { error: null, resetKeys: next } : { resetKeys: next };
  }

  componentDidCatch(error: Error) {
    console.error(error);
  }

  render() {
    const { error } = this.state;
    if (error) {
      if (typeof this.props.fallback === "function") return this.props.fallback(error);
      if (this.props.fallback !== undefined) return this.props.fallback;
      return <p className="text-sm text-destructive">Something went wrong rendering this.</p>;
    }
    return this.props.children;
  }
}

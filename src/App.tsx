import React from "react";
import i18n from "@/i18n";
import { extractErrorMessage } from "@/utils/errorUtils";
import { SessionManagerPage } from "@/components/sessions/SessionManagerPage";

interface ErrorBoundaryState {
  error: unknown;
}

export class AppErrorBoundary extends React.Component<
  { children: React.ReactNode },
  ErrorBoundaryState
> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: unknown): ErrorBoundaryState {
    return { error };
  }

  private handleReload = () => {
    this.setState({ error: null });
    if (typeof window !== "undefined" && window.location?.reload) {
      window.location.reload();
    }
  };

  render() {
    if (this.state.error) {
      const detail = extractErrorMessage(this.state.error);
      return (
        <div
          role="alert"
          className="flex h-screen w-full flex-col items-center justify-center gap-3 bg-background p-6 text-center text-foreground"
        >
          <p className="text-base font-semibold">
            {i18n.t("sessionManager.errorBoundaryTitle", {
              defaultValue: "页面渲染出现异常",
            })}
          </p>
          {detail && (
            <p className="max-w-lg break-all text-xs text-muted-foreground">
              {detail}
            </p>
          )}
          <button
            type="button"
            onClick={this.handleReload}
            className="rounded-md border border-border bg-secondary px-3 py-1.5 text-xs font-medium text-secondary-foreground hover:bg-accent"
          >
            {i18n.t("sessionManager.errorBoundaryReload", {
              defaultValue: "重新加载",
            })}
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

export default function App() {
  return (
    <AppErrorBoundary>
      <main className="h-screen min-h-0 overflow-hidden bg-background text-foreground">
        <SessionManagerPage appId="all" />
      </main>
    </AppErrorBoundary>
  );
}

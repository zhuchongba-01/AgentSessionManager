import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  AppLaunchSplash,
  useAppLaunchSplash,
} from "@/components/sessions/AppLaunchSplash";

function SplashHarness({
  isSettled,
  forceEnableInTest = true,
}: {
  isSettled: boolean;
  forceEnableInTest?: boolean;
}) {
  const phase = useAppLaunchSplash({ isSettled, forceEnableInTest });
  return (
    <div>
      <span data-testid="phase">{phase}</span>
      <AppLaunchSplash phase={phase} />
    </div>
  );
}

describe("AppLaunchSplash", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("bypasses the splash veil by default in unit test mode", () => {
    render(<SplashHarness isSettled={false} forceEnableInTest={false} />);
    expect(screen.getByTestId("phase")).toHaveTextContent("done");
    expect(screen.queryByTestId("app-launch-splash")).not.toBeInTheDocument();
  });

  it("transitions smoothly from veiled to revealing to done once scan settles", () => {
    const { rerender } = render(<SplashHarness isSettled={false} />);
    expect(screen.getByTestId("phase")).toHaveTextContent("veiled");
    expect(screen.getByTestId("app-launch-splash")).not.toHaveClass(
      "is-revealing",
    );

    act(() => {
      vi.advanceTimersByTime(500);
    });
    expect(screen.getByTestId("phase")).toHaveTextContent("veiled");

    rerender(<SplashHarness isSettled={true} />);
    expect(screen.getByTestId("phase")).toHaveTextContent("revealing");
    expect(screen.getByTestId("app-launch-splash")).toHaveClass("is-revealing");

    act(() => {
      vi.advanceTimersByTime(700);
    });
    expect(screen.getByTestId("phase")).toHaveTextContent("done");
    expect(screen.queryByTestId("app-launch-splash")).not.toBeInTheDocument();
  });

  it("auto-reveals after safety timeout even if initial scan is still pending", () => {
    render(<SplashHarness isSettled={false} />);
    act(() => {
      vi.advanceTimersByTime(2500);
    });
    expect(screen.getByTestId("phase")).toHaveTextContent("revealing");

    act(() => {
      vi.advanceTimersByTime(700);
    });
    expect(screen.getByTestId("phase")).toHaveTextContent("done");
  });
});

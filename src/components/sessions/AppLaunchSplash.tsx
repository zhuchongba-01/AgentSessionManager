import { useEffect, useState } from "react";
import appIcon from "@/icons/app-icon.png";
import { cn } from "@/lib/utils";

export type SplashPhase = "veiled" | "revealing" | "done";

const MIN_SPLASH_HOLD_MS = 480;
const MAX_SPLASH_HOLD_MS = 2400;
const SPLASH_REVEAL_MS = 680;

interface useAppLaunchSplashOptions {
  isSettled: boolean;
  forceEnableInTest?: boolean;
}

export function useAppLaunchSplash({
  isSettled,
  forceEnableInTest = false,
}: useAppLaunchSplashOptions): SplashPhase {
  const isTestEnv =
    !forceEnableInTest &&
    typeof import.meta !== "undefined" &&
    import.meta.env?.MODE === "test";

  const [phase, setPhase] = useState<SplashPhase>(() =>
    isTestEnv ? "done" : "veiled",
  );
  const [minHoldElapsed, setMinHoldElapsed] = useState(false);

  useEffect(() => {
    if (isTestEnv || phase !== "veiled") return;

    const minTimer = window.setTimeout(() => {
      setMinHoldElapsed(true);
    }, MIN_SPLASH_HOLD_MS);

    const maxTimer = window.setTimeout(() => {
      setPhase((current) => (current === "veiled" ? "revealing" : current));
    }, MAX_SPLASH_HOLD_MS);

    return () => {
      window.clearTimeout(minTimer);
      window.clearTimeout(maxTimer);
    };
  }, [isTestEnv, phase]);

  useEffect(() => {
    if (isTestEnv || phase !== "veiled") return;
    if (minHoldElapsed && isSettled) {
      setPhase("revealing");
    }
  }, [isSettled, isTestEnv, minHoldElapsed, phase]);

  useEffect(() => {
    if (phase !== "revealing") return;
    const doneTimer = window.setTimeout(() => {
      setPhase("done");
    }, SPLASH_REVEAL_MS);
    return () => window.clearTimeout(doneTimer);
  }, [phase]);

  return phase;
}

interface AppLaunchSplashProps {
  phase: SplashPhase;
}

export function AppLaunchSplash({ phase }: AppLaunchSplashProps) {
  if (phase === "done") return null;

  return (
    <div
      className={cn(
        "asm-launch-splash",
        phase === "revealing" && "is-revealing",
      )}
      aria-hidden="true"
      data-testid="app-launch-splash"
    >
      <div className="asm-launch-drag-strip" data-tauri-drag-region />
      <div className="asm-launch-glow asm-launch-glow-a" />
      <div className="asm-launch-glow asm-launch-glow-b" />
      <div className="asm-launch-emblem">
        <img
          src={appIcon}
          alt=""
          className="asm-launch-icon"
          draggable={false}
        />
        <div className="asm-launch-sheen" />
      </div>
    </div>
  );
}

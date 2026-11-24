import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface SegmentedOption<T extends string> {
  value: T;
  label: ReactNode;
  badge?: ReactNode;
  disabled?: boolean;
}

export interface SegmentedControlProps<T extends string> {
  value: T;
  onValueChange: (value: T) => void;
  options: readonly SegmentedOption<T>[];
  "aria-label"?: string;
  size?: "compact" | "regular";
  className?: string;
}

export function SegmentedControl<T extends string>({
  value,
  onValueChange,
  options,
  "aria-label": ariaLabel,
  size = "regular",
  className,
}: SegmentedControlProps<T>) {
  return (
    <div
      role="radiogroup"
      aria-label={ariaLabel}
      className={cn(
        "inline-flex shrink-0 items-center gap-0.5 rounded-control border border-border bg-subtle p-0.5",
        size === "compact" ? "h-7" : "h-8",
        className,
      )}
    >
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            disabled={opt.disabled}
            onClick={() => {
              if (!active && !opt.disabled) onValueChange(opt.value);
            }}
            className={cn(
              "inline-flex h-full items-center justify-center gap-1 whitespace-nowrap rounded-[4px] font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:pointer-events-none disabled:opacity-45",
              size === "compact" ? "px-2 text-caption" : "px-2.5 text-caption",
              active
                ? "bg-surface text-fg-1 shadow-sm"
                : "text-fg-2 hover:text-fg-1",
            )}
          >
            <span>{opt.label}</span>
            {opt.badge !== undefined && (
              <span className="tabular-nums text-fg-3">{opt.badge}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}

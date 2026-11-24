import * as React from "react";
import { Search, X } from "lucide-react";
import { cn } from "@/lib/utils";

export interface SearchFieldProps
  extends Omit<React.InputHTMLAttributes<HTMLInputElement>, "onChange"> {
  value: string;
  onValueChange: (value: string) => void;
  clearLabel?: string;
  trailing?: React.ReactNode;
  containerClassName?: string;
}

export const SearchField = React.forwardRef<HTMLInputElement, SearchFieldProps>(
  (
    {
      value,
      onValueChange,
      clearLabel = "清空搜索",
      trailing,
      containerClassName,
      className,
      ...props
    },
    ref,
  ) => {
    return (
      <div
        className={cn(
          "relative flex h-8 min-w-0 items-center gap-1.5 rounded-control border border-border bg-surface px-2.5 text-body text-fg-1 transition-colors focus-within:border-ring focus-within:ring-1 focus-within:ring-ring",
          containerClassName,
        )}
      >
        <Search aria-hidden className="h-3.5 w-3.5 shrink-0 text-fg-3" />
        <input
          ref={ref}
          type="search"
          value={value}
          onChange={(e) => onValueChange(e.target.value)}
          className={cn(
            "h-full min-w-0 flex-1 bg-transparent text-body text-fg-1 placeholder:text-fg-3 focus:outline-none [&::-webkit-search-cancel-button]:hidden",
            className,
          )}
          {...props}
        />
        {trailing}
        {value && (
          <button
            type="button"
            aria-label={clearLabel}
            onClick={() => onValueChange("")}
            className="inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-sm text-fg-3 hover:bg-subtle hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          >
            <X aria-hidden className="h-3.5 w-3.5" />
          </button>
        )}
      </div>
    );
  },
);
SearchField.displayName = "SearchField";

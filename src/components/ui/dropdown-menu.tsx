import * as React from "react";
import { createPortal } from "react-dom";
import { Check } from "lucide-react";
import { Slot } from "@radix-ui/react-slot";
import { cn } from "@/lib/utils";

interface DropdownMenuContextValue {
  open: boolean;
  setOpen: (open: boolean) => void;
  triggerRef: React.MutableRefObject<HTMLElement | null>;
}

const DropdownMenuContext =
  React.createContext<DropdownMenuContextValue | null>(null);

function useDropdownMenuContext() {
  const ctx = React.useContext(DropdownMenuContext);
  if (!ctx) {
    throw new Error("DropdownMenu components must be used inside DropdownMenu");
  }
  return ctx;
}

export interface DropdownMenuProps {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  children: React.ReactNode;
}

export function DropdownMenu({
  open: controlledOpen,
  defaultOpen = false,
  onOpenChange,
  children,
}: DropdownMenuProps) {
  const [uncontrolledOpen, setUncontrolledOpen] = React.useState(defaultOpen);
  const triggerRef = React.useRef<HTMLElement | null>(null);
  const isControlled = controlledOpen !== undefined;
  const open = isControlled ? controlledOpen : uncontrolledOpen;

  const setOpen = React.useCallback(
    (next: boolean) => {
      if (!isControlled) setUncontrolledOpen(next);
      onOpenChange?.(next);
    },
    [isControlled, onOpenChange],
  );

  const value = React.useMemo(
    () => ({ open, setOpen, triggerRef }),
    [open, setOpen],
  );

  return (
    <DropdownMenuContext.Provider value={value}>
      {children}
    </DropdownMenuContext.Provider>
  );
}

export interface DropdownMenuTriggerProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  asChild?: boolean;
}

export const DropdownMenuTrigger = React.forwardRef<
  HTMLButtonElement,
  DropdownMenuTriggerProps
>(({ asChild = false, onClick, ...props }, forwardedRef) => {
  const { open, setOpen, triggerRef } = useDropdownMenuContext();
  const Comp = asChild ? Slot : "button";

  const setRefs = React.useCallback(
    (node: HTMLButtonElement | null) => {
      triggerRef.current = node;
      if (typeof forwardedRef === "function") forwardedRef(node);
      else if (forwardedRef) forwardedRef.current = node;
    },
    [forwardedRef, triggerRef],
  );

  return (
    <Comp
      ref={setRefs}
      type={asChild ? undefined : "button"}
      aria-haspopup="menu"
      aria-expanded={open}
      data-state={open ? "open" : "closed"}
      onClick={(e: React.MouseEvent<HTMLButtonElement>) => {
        onClick?.(e);
        if (!e.defaultPrevented) {
          e.stopPropagation();
          setOpen(!open);
        }
      }}
      {...props}
    />
  );
});
DropdownMenuTrigger.displayName = "DropdownMenuTrigger";

export interface DropdownMenuContentProps
  extends React.HTMLAttributes<HTMLDivElement> {
  align?: "start" | "center" | "end";
  sideOffset?: number;
}

export const DropdownMenuContent = React.forwardRef<
  HTMLDivElement,
  DropdownMenuContentProps
>(
  (
    { align = "start", sideOffset = 4, className, style, children, ...props },
    forwardedRef,
  ) => {
    const { open, setOpen, triggerRef } = useDropdownMenuContext();
    const contentRef = React.useRef<HTMLDivElement | null>(null);
    const [coords, setCoords] = React.useState<{
      top: number;
      left: number;
    } | null>(null);

    const setRefs = React.useCallback(
      (node: HTMLDivElement | null) => {
        contentRef.current = node;
        if (typeof forwardedRef === "function") forwardedRef(node);
        else if (forwardedRef) forwardedRef.current = node;
      },
      [forwardedRef],
    );

    const updatePosition = React.useCallback(() => {
      const trigger = triggerRef.current;
      if (!trigger) return;
      const rect = trigger.getBoundingClientRect();
      const menuWidth = contentRef.current?.offsetWidth ?? 210;
      const menuHeight = contentRef.current?.offsetHeight ?? 180;
      let left = rect.left;
      if (align === "end") {
        left = rect.right - menuWidth;
      } else if (align === "center") {
        left = rect.left + rect.width / 2 - menuWidth / 2;
      }
      left = Math.max(8, Math.min(left, window.innerWidth - menuWidth - 8));
      let top = rect.bottom + sideOffset;
      if (
        top + menuHeight > window.innerHeight - 8 &&
        rect.top - sideOffset - menuHeight > 8
      ) {
        top = rect.top - sideOffset - menuHeight;
      }
      setCoords({ top, left });
    }, [align, sideOffset, triggerRef]);

    React.useLayoutEffect(() => {
      if (!open) return;
      updatePosition();
      const frame = requestAnimationFrame(updatePosition);
      return () => cancelAnimationFrame(frame);
    }, [open, updatePosition]);

    React.useEffect(() => {
      if (!open) return;
      const handlePointerDown = (event: MouseEvent) => {
        const target = event.target as Node | null;
        if (!target) return;
        if (contentRef.current?.contains(target)) return;
        if (triggerRef.current?.contains(target)) return;
        setOpen(false);
      };
      const handleKeyDown = (event: KeyboardEvent) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          setOpen(false);
          triggerRef.current?.focus();
        }
      };
      const handleViewport = () => updatePosition();

      document.addEventListener("mousedown", handlePointerDown);
      document.addEventListener("keydown", handleKeyDown);
      window.addEventListener("resize", handleViewport);
      window.addEventListener("scroll", handleViewport, true);
      return () => {
        document.removeEventListener("mousedown", handlePointerDown);
        document.removeEventListener("keydown", handleKeyDown);
        window.removeEventListener("resize", handleViewport);
        window.removeEventListener("scroll", handleViewport, true);
      };
    }, [open, setOpen, triggerRef, updatePosition]);

    if (!open || typeof document === "undefined") return null;

    return createPortal(
      <div
        ref={setRefs}
        role="menu"
        data-state="open"
        style={{
          position: "fixed",
          top: coords?.top ?? -9999,
          left: coords?.left ?? -9999,
          ...style,
        }}
        className={cn(
          "z-[90] min-w-[180px] overflow-hidden rounded-panel border border-border bg-popover p-1 text-fg-1 shadow-v7-md",
          className,
        )}
        onClick={(e) => e.stopPropagation()}
        {...props}
      >
        {children}
      </div>,
      document.body,
    );
  },
);
DropdownMenuContent.displayName = "DropdownMenuContent";

export interface DropdownMenuItemProps
  extends Omit<React.ButtonHTMLAttributes<HTMLButtonElement>, "onSelect"> {
  onSelect?: (event: Event) => void;
  inset?: boolean;
}

export const DropdownMenuItem = React.forwardRef<
  HTMLButtonElement,
  DropdownMenuItemProps
>(({ className, disabled, inset, onSelect, onClick, ...props }, ref) => {
  const { setOpen } = useDropdownMenuContext();
  return (
    <button
      ref={ref}
      type="button"
      role="menuitem"
      disabled={disabled}
      onClick={(e) => {
        onClick?.(e);
        if (disabled) return;
        const selectEvent = new CustomEvent("dropdown-select", {
          cancelable: true,
        });
        onSelect?.(selectEvent);
        if (!selectEvent.defaultPrevented) {
          setOpen(false);
        }
      }}
      className={cn(
        "relative flex w-full cursor-pointer select-none items-center gap-2 rounded-control px-2.5 py-1.5 text-left text-body text-fg-1 outline-none transition-colors hover:bg-subtle focus-visible:bg-subtle disabled:pointer-events-none disabled:opacity-45",
        inset && "ps-8",
        className,
      )}
      {...props}
    />
  );
});
DropdownMenuItem.displayName = "DropdownMenuItem";

export interface DropdownMenuCheckboxItemProps
  extends Omit<React.ButtonHTMLAttributes<HTMLButtonElement>, "onSelect"> {
  checked?: boolean;
  onCheckedChange?: (checked: boolean) => void;
  onSelect?: (event: Event) => void;
}

export const DropdownMenuCheckboxItem = React.forwardRef<
  HTMLButtonElement,
  DropdownMenuCheckboxItemProps
>(
  (
    {
      className,
      children,
      checked = false,
      disabled,
      onCheckedChange,
      onSelect,
      onClick,
      ...props
    },
    ref,
  ) => {
    const { setOpen } = useDropdownMenuContext();
    return (
      <button
        ref={ref}
        type="button"
        role="menuitemcheckbox"
        aria-checked={checked}
        disabled={disabled}
        onClick={(e) => {
          onClick?.(e);
          if (disabled) return;
          onCheckedChange?.(!checked);
          const selectEvent = new CustomEvent("dropdown-select", {
            cancelable: true,
          });
          onSelect?.(selectEvent);
          if (!selectEvent.defaultPrevented) {
            setOpen(false);
          }
        }}
        className={cn(
          "relative flex w-full cursor-pointer select-none items-center gap-2 rounded-control py-1.5 pe-2.5 ps-7 text-left text-body text-fg-1 outline-none transition-colors hover:bg-subtle focus-visible:bg-subtle disabled:pointer-events-none disabled:opacity-45",
          className,
        )}
        {...props}
      >
        <span className="absolute start-2 inline-flex h-3.5 w-3.5 items-center justify-center">
          {checked && (
            <Check aria-hidden className="h-3.5 w-3.5 text-action-text" />
          )}
        </span>
        {children}
      </button>
    );
  },
);
DropdownMenuCheckboxItem.displayName = "DropdownMenuCheckboxItem";

export function DropdownMenuSeparator({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      role="separator"
      className={cn("-mx-1 my-1 h-px bg-border", className)}
      {...props}
    />
  );
}

export function DropdownMenuLabel({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "px-2.5 py-1 text-caption font-semibold text-fg-3",
        className,
      )}
      {...props}
    />
  );
}

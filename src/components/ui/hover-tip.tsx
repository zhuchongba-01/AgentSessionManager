import type { ReactElement, ReactNode } from "react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";

export interface HoverTipProps {
  content?: ReactNode;
  side?: "top" | "right" | "bottom" | "left";
  align?: "start" | "center" | "end";
  children: ReactElement;
}

export function HoverTip({
  content,
  side = "bottom",
  align = "center",
  children,
}: HoverTipProps) {
  if (!content) return children;
  return (
    <Tooltip delayDuration={250}>
      <TooltipTrigger asChild>{children}</TooltipTrigger>
      <TooltipContent side={side} align={align} className="max-w-[320px]">
        {content}
      </TooltipContent>
    </Tooltip>
  );
}

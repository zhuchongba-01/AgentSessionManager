import { createPortal } from "react-dom";
import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/utils";
import type { SessionMessage } from "@/types";

interface SessionMinimapProps {
  messages: SessionMessage[];
  messageIndexes: number[];
  visibleMessageIndexes: number[];
  onTickClick: (index: number) => void;
}

interface MinimapTurn {
  messageIndex: number;
  user: string;
  reply: string;
}

const SLOT_HEIGHT = 12;
const TICK_WIDTH = 6;
const WAVE_AMPLITUDE = 19;
const WAVE_RADIUS = 3.25;
const MIN_VISIBLE_TICKS = 5;
const MAX_VISIBLE_TICKS = 28;
const DEFAULT_VISIBLE_TICKS = 18;

const clampText = (text: string, max: number) =>
  text.length > max ? text.slice(0, max) + "…" : text;

const clamp = (value: number, min: number, max: number) =>
  Math.min(Math.max(value, min), max);

/**
 * Compact turn navigator modeled after Codex's conversation gutter.
 *
 * It receives the same filtered user-message indexes as the TOC, so injected
 * IDE/developer context never becomes a tick. Long conversations render only
 * a bounded window around the current reading position instead of compressing
 * hundreds of messages into an unusable stripe.
 */
export function SessionMinimap({
  messages,
  messageIndexes,
  visibleMessageIndexes,
  onTickClick,
}: SessionMinimapProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [containerHeight, setContainerHeight] = useState(0);
  const [hoveredTurnIndex, setHoveredTurnIndex] = useState<number | null>(null);
  const [hoverCard, setHoverCard] = useState<{
    top: number;
    left: number;
    user: string;
    reply: string;
  } | null>(null);

  const turns = useMemo<MinimapTurn[]>(
    () =>
      messageIndexes.flatMap((messageIndex, turnIndex) => {
        const message = messages[messageIndex];
        if (!message) return [];
        const nextMessageIndex =
          messageIndexes[turnIndex + 1] ?? messages.length;
        const reply = messages
          .slice(messageIndex + 1, nextMessageIndex)
          .filter((item) => item.role.toLowerCase() === "assistant")
          .map((item) => item.content.slice(0, 220))
          .join("\n")
          .slice(0, 220);
        return [{ messageIndex, user: message.content, reply }];
      }),
    [messageIndexes, messages],
  );

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    const updateHeight = () => setContainerHeight(element.clientHeight);
    const observer = new ResizeObserver(updateHeight);
    observer.observe(element);
    updateHeight();
    return () => observer.disconnect();
  }, []);

  const currentTurnIndexes = useMemo(() => {
    const activeTurns = new Set<number>();
    visibleMessageIndexes.forEach((messageIndex) => {
      let nearest = -1;
      turns.forEach((turn, index) => {
        if (turn.messageIndex <= messageIndex) nearest = index;
      });
      if (nearest >= 0) activeTurns.add(nearest);
    });
    return activeTurns;
  }, [turns, visibleMessageIndexes]);

  const currentTurnIndex = useMemo(() => {
    if (currentTurnIndexes.size === 0) return 0;
    const activeTurns = Array.from(currentTurnIndexes);
    return Math.round(
      (activeTurns[0] + activeTurns[activeTurns.length - 1]) / 2,
    );
  }, [currentTurnIndexes]);

  const visibleCapacity =
    containerHeight > 0
      ? clamp(
          Math.floor((containerHeight - 16) / SLOT_HEIGHT),
          MIN_VISIBLE_TICKS,
          MAX_VISIBLE_TICKS,
        )
      : DEFAULT_VISIBLE_TICKS;
  const visibleStart = clamp(
    currentTurnIndex - Math.floor(visibleCapacity / 2),
    0,
    Math.max(0, turns.length - visibleCapacity),
  );
  const visibleTurns = turns
    .slice(visibleStart, visibleStart + visibleCapacity)
    .map((turn, offset) => ({ turn, turnIndex: visibleStart + offset }));

  const enter = (
    turnIndex: number,
    turn: MinimapTurn,
    element: HTMLElement,
  ) => {
    setHoveredTurnIndex(turnIndex);
    const rect = element.getBoundingClientRect();
    const cardWidth = Math.min(430, Math.max(260, window.innerWidth - 28));
    const estimatedCardHeight = 138;
    setHoverCard({
      top: clamp(
        rect.top + rect.height / 2 - estimatedCardHeight / 2,
        12,
        Math.max(12, window.innerHeight - estimatedCardHeight - 12),
      ),
      left: clamp(
        rect.right + 10,
        12,
        Math.max(12, window.innerWidth - cardWidth - 12),
      ),
      user: clampText(turn.user, 90),
      reply: turn.reply.trim()
        ? clampText(turn.reply, 220)
        : "（暂无回复内容）",
    });
  };

  const leave = () => {
    setHoveredTurnIndex(null);
    setHoverCard(null);
  };

  if (turns.length === 0) return null;

  return (
    <div ref={containerRef} className="asm-minimap" onMouseLeave={leave}>
      <div className="asm-minimap-track">
        {visibleTurns.map(({ turn, turnIndex }) => {
          const isHovered = turnIndex === hoveredTurnIndex;
          const isCurrent = currentTurnIndexes.has(turnIndex);
          const distance =
            hoveredTurnIndex === null
              ? Number.POSITIVE_INFINITY
              : Math.abs(turnIndex - hoveredTurnIndex);
          const wave = Math.max(0, 1 - distance / WAVE_RADIUS);
          const width =
            hoveredTurnIndex === null
              ? TICK_WIDTH
              : TICK_WIDTH + wave * WAVE_AMPLITUDE;
          return (
            <button
              key={turn.messageIndex}
              type="button"
              className="asm-minimap-slot"
              aria-label={`跳转到第 ${turnIndex + 1} 个用户问题`}
              onMouseEnter={(event) =>
                enter(turnIndex, turn, event.currentTarget)
              }
              onFocus={(event) => enter(turnIndex, turn, event.currentTarget)}
              onBlur={leave}
              onClick={() => onTickClick(turn.messageIndex)}
            >
              <span
                className={cn(
                  "asm-minimap-tick",
                  isCurrent && "is-current",
                  isHovered && "is-hover",
                )}
                style={{ width }}
              />
            </button>
          );
        })}
      </div>
      {hoverCard &&
        createPortal(
          <div
            role="tooltip"
            className="asm-minimap-pop"
            style={{ top: hoverCard.top, left: hoverCard.left }}
          >
            <div className="asm-minimap-pop-user">{hoverCard.user}</div>
            <div className="asm-minimap-pop-reply">{hoverCard.reply}</div>
          </div>,
          document.body,
        )}
    </div>
  );
}

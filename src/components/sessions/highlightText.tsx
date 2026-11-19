import type { ReactNode } from "react";

export const EMPTY_HIGHLIGHT_TERMS: string[] = [];

const escapeRegExp = (value: string) =>
  value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

export function highlightText(text: string, terms: string[]): ReactNode {
  if (!text || terms.length === 0) return text;
  const cleaned = Array.from(
    new Set(terms.map((term) => term.trim()).filter(Boolean)),
  ).sort((a, b) => b.length - a.length);
  if (cleaned.length === 0) return text;

  const pattern = new RegExp(`(${cleaned.map(escapeRegExp).join("|")})`, "gi");
  const parts = text.split(pattern);
  if (parts.length <= 1) return text;

  const normalized = new Set(cleaned.map((term) => term.toLocaleLowerCase()));
  return parts.map((part, index) =>
    part && normalized.has(part.toLocaleLowerCase()) ? (
      <mark key={index} className="asm-search-mark">
        {part}
      </mark>
    ) : (
      part
    ),
  );
}

import { useCallback } from "react";

export const formatMoneyCompact = (usd: number): string => {
  if (!Number.isFinite(usd) || usd < 0) return "$0";
  if (usd === 0) return "$0";
  if (usd < 0.001) return "<$0.001";
  if (usd < 0.01) return `$${usd.toFixed(3)}`;
  if (usd < 10) return `$${usd.toFixed(2)}`;
  if (usd < 100) return `$${usd.toFixed(1)}`;
  return `$${Math.round(usd)}`;
};

export function useMoneyFormat() {
  const moneyCompact = useCallback(
    (usd: number) => formatMoneyCompact(usd),
    [],
  );
  return { moneyCompact };
}

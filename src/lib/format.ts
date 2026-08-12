import type { DailyUsageBucket, RateLimitWindow } from "../codex-types";

export function formatNumber(value?: number | null) {
  if (value == null || Number.isNaN(value)) return "—";
  return new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

export function formatWindow(minutes?: number | null) {
  if (!minutes) return "Quota";
  if (minutes === 300) return "5h";
  if (minutes === 10080) return "Weekly";
  if (minutes % 1440 === 0) return `${minutes / 1440}d`;
  if (minutes % 60 === 0) return `${minutes / 60}h`;
  return `${minutes}m`;
}

export function remainingPercent(window?: RateLimitWindow | null) {
  const used = Math.max(0, Math.min(100, window?.usedPercent ?? 0));
  return 100 - used;
}

export function formatReset(resetsAt?: number | null) {
  if (!resetsAt) return "Reset time unavailable";
  const delta = Math.max(0, resetsAt * 1000 - Date.now());
  const totalMinutes = Math.floor(delta / 60000);
  const days = Math.floor(totalMinutes / 1440);
  const hours = Math.floor((totalMinutes % 1440) / 60);
  const minutes = totalMinutes % 60;
  if (days > 0) return `resets in ${days}d ${hours}h`;
  if (hours > 0) return `resets in ${hours}h ${minutes}m`;
  return `resets in ${minutes}m`;
}

export function todayTokens(buckets?: DailyUsageBucket[] | null) {
  if (!buckets?.length) return null;
  const now = new Date();
  const today = [
    now.getFullYear(),
    String(now.getMonth() + 1).padStart(2, "0"),
    String(now.getDate()).padStart(2, "0"),
  ].join("-");
  return buckets.find((item) => item.startDate === today)?.tokens ?? buckets[buckets.length - 1]?.tokens ?? null;
}

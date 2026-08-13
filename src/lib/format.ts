import type { DailyUsageBucket, RateLimitWindow } from "../codex-types";

export function formatNumber(value?: number | null) {
  if (value == null || Number.isNaN(value)) return "—";
  return new Intl.NumberFormat("zh-CN", { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

export function formatWindow(minutes?: number | null) {
  if (!minutes) return "配额";
  if (minutes === 300) return "5 小时";
  if (minutes === 10080) return "每周";
  if (minutes % 1440 === 0) return `${minutes / 1440} 天`;
  if (minutes % 60 === 0) return `${minutes / 60} 小时`;
  return `${minutes} 分钟`;
}

export function remainingPercent(window?: RateLimitWindow | null) {
  const used = Math.max(0, Math.min(100, window?.usedPercent ?? 0));
  return 100 - used;
}

export function formatReset(resetsAt?: number | null) {
  if (!resetsAt) return "重置时间不可用";
  const delta = Math.max(0, resetsAt * 1000 - Date.now());
  const totalMinutes = Math.floor(delta / 60000);
  const days = Math.floor(totalMinutes / 1440);
  const hours = Math.floor((totalMinutes % 1440) / 60);
  const minutes = totalMinutes % 60;
  if (days > 0) return `${days} 天 ${hours} 小时后重置`;
  if (hours > 0) return `${hours} 小时 ${minutes} 分钟后重置`;
  return `${minutes} 分钟后重置`;
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

import type {
  RateLimitBucket,
  RateLimitBucketUpdate,
  RateLimitsResult,
} from "./codex-types";

export interface QuotaWindow {
  id: string;

  limitId: string;
  limitName: string;

  label: string;

  usedPercent: number;
  remainingPercent: number;

  windowDurationMins: number;
  resetsAt: number;

  planType: string | null;
}

export function formatWindowLabel(
  minutes: number,
): string {
  if (minutes === 300) {
    return "5 小时";
  }

  if (minutes === 1440) {
    return "每日";
  }

  if (minutes === 10080) {
    return "每周";
  }

  if (minutes % 10080 === 0) {
    const weeks = minutes / 10080;
    return `${weeks} 周`;
  }

  if (minutes % 1440 === 0) {
    const days = minutes / 1440;
    return `${days} 天`;
  }

  if (minutes % 60 === 0) {
    const hours = minutes / 60;
    return `${hours} 小时`;
  }

  return `${minutes} 分钟`;
}

export function normalizeRateLimits(
  result: RateLimitsResult | null,
): QuotaWindow[] {
  if (!result) {
    return [];
  }

  const byId =
    result.rateLimitsByLimitId ?? {};

  const buckets =
    Object.keys(byId).length > 0
      ? Object.values(byId)
      : result.rateLimits
        ? [result.rateLimits]
        : [];

  return buckets
    .flatMap((bucket) => {
      const windows = [
        ["primary", bucket.primary],
        ["secondary", bucket.secondary],
      ] as const;

      return windows.flatMap(
        ([type, window]) => {
          if (!window) {
            return [];
          }

          return [
            {
              id: `${bucket.limitId}:${type}`,

              limitId: bucket.limitId,

              limitName:
                bucket.limitName ??
                bucket.limitId,

              label: formatWindowLabel(
                window.windowDurationMins,
              ),

              usedPercent:
                window.usedPercent,

              remainingPercent:
                Math.max(
                  0,
                  100 - window.usedPercent,
                ),

              windowDurationMins:
                window.windowDurationMins,

              resetsAt:
                window.resetsAt,

              planType:
                bucket.planType,
            },
          ];
        },
      );
    })
    .sort(
      (a, b) =>
        a.windowDurationMins -
        b.windowDurationMins,
    );
}

export function mergeRateLimitsUpdate(
  current: RateLimitsResult | null,
  update: RateLimitBucketUpdate,
): RateLimitsResult {
  const currentById =
    current?.rateLimitsByLimitId ?? {};

  const previous =
    currentById[update.limitId] ??
    (
      current?.rateLimits?.limitId ===
      update.limitId
        ? current.rateLimits
        : undefined
    );

  const merged: RateLimitBucket = {
    limitId: update.limitId,

    limitName:
      update.limitName !== undefined
        ? update.limitName
        : previous?.limitName ?? null,

    planType:
      update.planType !== undefined
        ? update.planType
        : previous?.planType ?? null,

    credits:
      update.credits !== undefined
        ? update.credits
        : previous?.credits ?? null,

    individualLimit:
      update.individualLimit !== undefined
        ? update.individualLimit
        : previous?.individualLimit ?? null,

    rateLimitReachedType:
      update.rateLimitReachedType !== undefined
        ? update.rateLimitReachedType
        : previous?.rateLimitReachedType ?? null,

    primary:
      update.primary !== undefined
        ? update.primary
        : previous?.primary ?? null,

    secondary:
      update.secondary !== undefined
        ? update.secondary
        : previous?.secondary ?? null,
  };

  return {
    rateLimitResetCredits:
      current?.rateLimitResetCredits ?? null,

    rateLimits:
      !current?.rateLimits ||
      current.rateLimits.limitId ===
        update.limitId
        ? merged
        : current.rateLimits,

    rateLimitsByLimitId: {
      ...currentById,
      [update.limitId]: merged,
    },
  };
}

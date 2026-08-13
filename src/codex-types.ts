export interface RateLimitWindow {
  usedPercent: number;
  windowDurationMins: number;
  resetsAt: number;
}

export interface Credits {
  balance: string;
  hasCredits: boolean;
  unlimited: boolean;
}

export interface RateLimitBucket {
  limitId: string;
  limitName: string | null;
  planType: string | null;

  credits: Credits | null;

  individualLimit: unknown | null;
  rateLimitReachedType: string | null;

  primary: RateLimitWindow | null;
  secondary: RateLimitWindow | null;
}

export interface RateLimitResetCredits {
  availableCount: number;
  credits: unknown[];
}

export interface RateLimitsResult {
  rateLimitResetCredits: RateLimitResetCredits | null;

  rateLimits: RateLimitBucket | null;

  rateLimitsByLimitId:
    Record<string, RateLimitBucket>;
}

export interface DailyUsageBucket {
  startDate: string;
  tokens: number;
}

export interface UsageSummary {
  currentStreakDays: number | null;
  lifetimeTokens: number | null;
  longestRunningTurnSec: number | null;
  longestStreakDays: number | null;
  peakDailyTokens: number | null;
}

export interface UsageResult {
  dailyUsageBuckets: DailyUsageBucket[] | null;
  summary: UsageSummary | null;
}

export interface CodexAccount {
  email: string | null;
  planType: string | null;
  type?: string | null;
}

export interface AccountReadResult {
  account: CodexAccount | null;
}

export type CodexAccountState =
  | "unknown"
  | "signedIn"
  | "signedOut"
  | "error";

export interface CodexSnapshot {
  codexPath: string;
  fetchedAt: number;

  account: AccountReadResult  | null;
  accountError: string | null;

  accountState: CodexAccountState;

  rateLimits: RateLimitsResult | null;

  usage: UsageResult | null;
  usageError: string | null;
}

/**
 * rateLimits/updated 不保证返回完整 bucket，
 * 所以事件 payload 使用 Partial。
 */
export type RateLimitBucketUpdate =
  Partial<Omit<RateLimitBucket, "limitId">> & {
    limitId: string;
  };

export interface RateLimitsUpdatedPayload {
  rateLimits: RateLimitBucketUpdate;
}

export interface AccountUpdatedPayload {
  account?: CodexAccount | null;
  planType?: string | null;
  email?: string | null;
  type?: string | null;
}

export type CodexConnectionPhase =
  | "disconnected"
  | "connecting"
  | "initializing"
  | "ready"
  | "reconnecting";

export interface CodexConnectionStatus {
  phase: CodexConnectionPhase;

  generation: number;

  attempt: number;

  retryInMs: number | null;

  lastError: string | null;

  codexPath: string | null;
}

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

export interface MonitorSettings {
  notifyThresholds: number[];
  notifyQuotaReset: boolean;
  launchAtStartup: boolean;
  startMinimized: boolean;
  closeToTray: boolean;
  usageRefreshPolicy: string;
  lastNotifiedThreshold: Record<string, number>;
  lastSeenResetAt: Record<string, number>;
  lastResetNotifiedAt: Record<string, number>;
}

export interface UsageSchedulerStatus {
  policy: string;
  mode: string;
  watcherActive: boolean;
  pendingReconciliation: boolean;
  fallbackSeconds: number;
  lastRefreshAt: number | null;
  lastLocalActivityAt: number | null;
  refreshing: boolean;
  refreshReason: string | null;
  refreshStartedAt: number | null;
  refreshGeneration: number | null;
  queuedRefresh: boolean;
}

export interface UsageRefreshCompletedPayload {
  refreshGeneration: number;
  reason: string;
  success: boolean;
  error: string | null;
}

export interface UsageDataInvalidatedPayload {
  reason: string;
  invalidatedAt: number;
}

export type CategoryUsagePeriod = "day" | "quota_week";
export type ServerUsageCapability = "available" | "unavailable" | "syncing";
export type UsageDataStatus = "observed" | "estimated" | "insufficient_data";

export interface UsageMetric {
  status: UsageDataStatus;
  value: number | null;
  sampleCount: number;
  changeCount: number;
  confidence: UsageConfidence;
  source: string;
}

export interface TokenUsageMetric {
  status: UsageDataStatus;
  valueTokens: number;
  sampleCount: number;
  confidence: UsageConfidence;
  source: string;
}

export interface CategoryTokenEstimate {
  status: UsageDataStatus;
  estimatedTokens: number | null;
  remainingTokens: number | null;
  currentTokens: number;
  totalCategoryTokens: number;
  observedSampleCount: number;
  validSampleCount: number;
  observedTokens: number;
  observedQuotaPercent: number;
  cumulativeObservedQuotaDelta: number;
  coverageRatio: number;
  preObservationTokens: number;
  eligibleTokens: number;
  pendingTokens: number;
  rejectedSampleCount: number;
  boundaryOverlapCount: number;
  boundaryOverlapRatio: number;
  dispersionRatio: number;
  rejectionReasons: string[];
  externalUsageRisk: boolean;
  confidence: UsageConfidence;
  source: string;
}

export interface CategoryUsageItem {
  model: string;
  reasoningEffort: string;
  speedMode: "standard" | "fast_requested" | "unknown";
  fast: boolean;
  turnCount: number;
  tokens: number;
  tokenSource: "local_rollout";
  serverEstimatedCreditsMicros: number | null;
  creditSource: "app_server" | "unavailable";
  weeklyQuotaPercent: number | null;
  weeklyEstimate: CategoryTokenEstimate | null;
}

export interface CategoryUsageQuotaWindow {
  limitId: string;
  window: string;
  usedPercent: number;
  remainingPercent: number;
  windowDurationMins: number;
  resetsAt: number | null;
}

export interface CategoryUsage {
  period: CategoryUsagePeriod;
  periodStart: number;
  periodEnd: number;
  periodSource: "local_day" | "quota_window" | "insufficient_data";
  accountKey: string | null;
  officialTokens: number | null;
  localTokens: number;
  serverUsageCapability: ServerUsageCapability;
  quotaWindow: CategoryUsageQuotaWindow | null;
  quotaUsage: UsageMetric | null;
  tokenUsage: TokenUsageMetric;
  categories: CategoryUsageItem[];
}

export interface HistoryLimit {
  limitId: string;
  limitName: string;
  window: string;
  windowDurationMins: number;
  usedPercent: number;
  resetsAt: number | null;
}

export interface UsageHistoryEntry {
  timestamp: number;
  limits: Record<string, HistoryLimit>;
  lifetimeTokens: number | null;
}

export type UsageRange = "7d" | "15d" | "30d" | "90d" | "all";
export type UsageBreakdown = "model" | "reasoning" | "speed" | "tokenType";

export type UsageSource = "official" | "local" | "derived" | "derived_estimate" | "estimated";
export type UsageConfidence = "high" | "medium" | "low" | "unknown";

export type UsageAccountScope =
  | { type: "single"; accountKey: string }
  | { type: "all" };

export interface UsageAnalyticsQuery {
  accountScope: UsageAccountScope;
  from: string;
  to: string;
  timezone: string;
  breakdown: "model" | "reasoning" | "speed" | "account" | "tokenType";
}

export type UsageReasoningEffort = "low" | "medium" | "high" | "xhigh" | "ultra" | "unknown";
export type UsageSpeedMode = "standard" | "fast_requested" | "unknown";

export interface TurnUsageRecord {
  accountKey: string;
  threadId: string;
  turnId: string;
  startedAt: number;
  completedAt: number | null;
  model: string | null;
  reasoningEffort: UsageReasoningEffort;
  speedMode: UsageSpeedMode;
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  reasoningOutputTokens: number;
  rawTotalTokens: number;
  estimatedCredits: number | null;
  source: "rollout" | "app-server";
  confidence: Exclude<UsageConfidence, "unknown">;
}

export interface UsageBreakdownItem {
  key: string;
  label: string;
  rawTokens: number;
  rawTokenShare: number;
  estimatedCredits: number | null;
  attributedQuotaPercent: number | null;
  quotaShare: number | null;
  source: UsageSource;
  confidence: UsageConfidence;
}

export interface DailyUsageAnalytics {
  date: string;
  localTokens: number;
  rawTokens: number;
  officialTokens: number | null;
  estimatedCredits: number | null;
  observedQuotaPercent: number | null;
  attributableQuotaPercent: number | null;
  unattributedQuotaPercent: number | null;
  turnCount: number;
  categories: Record<string, {
    rawTokens: number;
    estimatedCredits: number | null;
    attributedQuotaPercent: number | null;
    source: UsageSource;
    confidence: UsageConfidence;
  }>;
}

export interface UsageAnalyticsV1 {
  scope: UsageAccountScope;
  period: { from: string; to: string; timezone: string };
  summary: {
    rawTokens: number;
    estimatedCredits: number | null;
    observedQuotaPercent: number | null;
    attributableQuotaPercent: number | null;
    unattributedQuotaPercent: number | null;
    activeDays: number;
    turnCount: number;
  };
  breakdownItems: UsageBreakdownItem[];
  timeline: DailyUsageAnalytics[];
  accounts: Array<{
    accountKey: string;
    displayName: string | null;
    rawTokens: number;
    estimatedCredits: number | null;
    observedQuotaPercent: number | null;
    currentUsedPercent: number | null;
    remainingPercent: number | null;
    resetsAt: number | null;
    activeDays: number;
    turnCount: number;
  }>;
}

export interface UsageAnalyticsPoint {
  date: string;
  officialTokens: number | null;
  localTokens: number;
  unattributedTokens: number;
  categoryValues: Record<string, number>;
}

export interface UsageAnalytics {
  accountKey: string | null;
  range: UsageRange;
  breakdown: UsageBreakdown;
  categories: string[];
  points: UsageAnalyticsPoint[];
  turnCount: number;
  officialTotalTokens: number;
  localTotalTokens: number;
  estimatedRemainingTokens: number | null;
  estimateSampleCount: number;
}

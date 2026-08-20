import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import {
  invoke,
} from "@tauri-apps/api/core";

import {
  listen,
} from "@tauri-apps/api/event";

import type {
  CodexConnectionStatus,
  CodexAccountState,
  CodexSnapshot,
  CategoryUsage,
  DailyUsageBucket,
  MonitorSettings,
  RateLimitsUpdatedPayload,
  UsageDataInvalidatedPayload,
  UsageAnalytics,
  UsageBreakdown,
  UsageRange,
  UsageSchedulerStatus,
  CollectorDataHealth,
} from "./codex-types";

import { collectorIpc } from "./collector-ipc";

import {
  mergeRateLimitsUpdate,
  normalizeRateLimits,
} from "./codex-normalize";

import {
  formatNumber,
  formatReset,
} from "./lib/format";

import codexIconBody from "../assets/branding/app/app-icon-master.png";

function confidenceLabel(value: string): string {
  switch (value) {
    case "high":
      return "高";
    case "medium":
      return "中";
    case "low":
      return "低";
    default:
      return "未知";
  }
}

function estimatorReasonLabel(value: string): string {
  switch (value) {
    case "insufficient_samples":
      return "样本不足";
    case "insufficient_valid_samples":
      return "有效样本不足";
    case "insufficient_observed_tokens":
      return "有效 Token 不足";
    case "insufficient_observed_quota":
      return "累计额度变化不足";
    case "insufficient_quota_span":
      return "累计额度变化不足";
    case "coverage_below_threshold":
      return "Token coverage 不足";
    case "insufficient_coverage":
      return "Token coverage 不足";
    case "eligible_tokens_missing":
      return "没有可归因 Token 分母";
    case "insufficient_eligible_tokens":
      return "没有可归因 Token 分母";
    case "accounting_inconsistent":
      return "Token 账本不一致";
    case "legacy_unverified":
      return "历史数据尚未验证";
    case "account_data_rebuilding":
      return "账号数据正在重建";
    case "source_incomplete":
      return "Token 采集尚未追平";
    case "long_observation_gap":
      return "long gap，估算已阻断";
    case "unresolved_source":
      return "存在 unresolved source，未进入估算";
    case "short_observation_gap":
      return "短 observation gap，置信度降低";
    case "unattributed_quota_delta":
      return "额度变化无法直接归因到分类";
    case "boundary_overlap":
      return "boundary overlap 太多";
    case "boundary_ambiguity":
      return "跨 boundary 的 Token 不确定";
    case "mixed_category":
      return "同一额度 step 包含多个分类";
    case "mixed_category_unresolved":
      return "同一额度 step 包含多个分类";
    case "no_category_token":
      return "额度 step 内没有可归因 Token";
    case "dispersion_too_high":
      return "样本离散度过高";
    case "excessive_dispersion":
      return "样本离散度过高";
    case "pending_tokens":
      return "当前仍有未闭合 Token";
    case "external_usage_risk":
      return "存在外部或未归因 Token 风险";
    case "sanity_check_failed":
      return "数学一致性检查失败";
    case "quota_window_missing":
      return "缺少当前周额度窗口";
    default:
      return value;
  }
}

function reasoningLabel(value: string): string {
  switch (value) {
    case "low":
      return "低";
    case "medium":
      return "中";
    case "high":
      return "高";
    case "xhigh":
      return "极高";
    case "ultra":
      return "超高";
    default:
      return "未知";
  }
}

function modelLabel(value: string): string {
  return value === "unknown" ? "未知模型" : value;
}

function refreshPolicyLabel(value: string): string {
  switch (value) {
    case "adaptive":
      return "自适应实时";
    case "15s":
      return "每 15 秒";
    case "30s":
      return "每 30 秒";
    case "1m":
      return "每 1 分钟";
    case "3m":
      return "每 3 分钟";
    case "5m":
      return "每 5 分钟";
    case "5s":
      return "每 5 秒";
    default:
      return value;
  }
}

/*
 * ============================================================
 * Quota Card
 * ============================================================
 */
function QuotaCard({
  limitName,
  title,
  usedPercent,
  remainingPercent,
  resetsAt,
  children,
}: {
  limitName: string;
  title: string;
  usedPercent: number;
  remainingPercent: number;
  resetsAt: number | null;
  children?: ReactNode;
}) {
  const used =
    Math.max(
      0,
      Math.min(
        100,
        usedPercent,
      ),
    );

  const remaining =
    Math.max(
      0,
      Math.min(
        100,
        remainingPercent,
      ),
    );

  return (
    <section className="quota-card">
      <div className="row quota-head">
        <div>
          <div className="eyebrow">
            {limitName}
          </div>

          <h2>
            {title}
          </h2>
        </div>

        <div className="quota-number">
          {Math.round(used)}%
        </div>
      </div>

      <div
        className="bar"
        aria-label={`已使用 ${used}%`}
      >
        <div
          className="bar-fill"
          style={{
            width: `${used}%`,
          }}
        />
      </div>

      <div className="row muted small">
        <span>
          {Math.round(remaining)}%
          {" "}
          剩余
        </span>

        <span>
          {formatReset(resetsAt)}
        </span>
      </div>

      {children}
    </section>
  );
}

function WeeklyTokenEstimateSection({ usage }: { usage: CategoryUsage | null }) {
  const categories = usage?.categories ?? [];
  const statusText = (status: string) => (
    status === "estimated" ? "预估" : "参数过少，暂无法准确预估"
  );

  return (
    <div className="quota-estimate-section">
      <div className="quota-estimate-heading">
        <h2>各模型预计完整周额度 Token 量</h2>
      </div>

      {usage?.periodSource === "insufficient_data" ? (
        <p className="quota-estimate-warning">
          未观测到可由窗口时长与重置时间确认的当前 Codex 周额度窗口；严格周统计暂不使用自然周或最近 7 天回退数据。
        </p>
      ) : null}

      {usage?.dataHealth && usage.dataHealth.status !== "verified" ? (
        <p className="quota-estimate-warning">
          当前账号数据状态为 {usage.dataHealth.status}，估算已暂停；未追平数据源 {usage.dataHealth.sourceIncompleteCount} 个、source lag {usage.dataHealth.sourceLagSeconds} 秒，缺失 Timeline {usage.dataHealth.missingTimelineTurns} 条、孤立 Timeline {usage.dataHealth.orphanTimelineSamples} 条、解析错误 {usage.dataHealth.parseErrorCount} 条。
        </p>
      ) : null}

      {!categories.length ? (
        <p className="muted small daily-model-usage-empty">
          {usage ? "当前额度周期暂无本地轮次 Token 记录。" : "正在读取本额度周期…"}
        </p>
      ) : (
        <div className="quota-estimate-list">
          {categories.map((category) => {
            const estimate = category.weeklyEstimate;
            const estimated = estimate?.status === "estimated" && estimate.estimatedTokens != null;
            return (
              <div
                className="quota-estimate-item"
                key={`${category.model}:${category.reasoningEffort}:${category.speedMode}`}
              >
                <div className="quota-estimate-row">
                  <div className="daily-model-usage-label">
                    <strong>
                      {modelLabel(category.model)}
                      <span className="daily-model-usage-reasoning">
                        ({reasoningLabel(category.reasoningEffort)})
                      </span>
                      {category.fast ? <span className="fast-badge" title="快速模式" aria-label="快速模式">⚡</span> : null}
                    </strong>
                    <span className="muted tiny">
                      {category.turnCount} 个轮次 · {formatNumber(category.tokens)} 真实 Token
                    </span>
                  </div>
                  <div className="daily-model-usage-values">
                    <strong>{estimated ? `≈ ${formatNumber(estimate.estimatedTokens!)} Token` : statusText(estimate?.status ?? "insufficient_data")}</strong>
                    {estimated ? (
                      <span className="muted tiny">预估 · {estimate!.validSampleCount} 个有效额度样本 · 置信度 {confidenceLabel(estimate!.confidence)}</span>
                    ) : null}
                  </div>
                </div>
                {estimate ? (
                  <details className="quota-estimate-diagnostics">
                    <summary>诊断详情</summary>
                    <div className="quota-estimate-diagnostics-grid">
                      <span>当前分类 Token</span><strong>{formatNumber(estimate.currentTokens)}</strong>
                      <span>本周分类总 Token</span><strong>{formatNumber(estimate.totalCategoryTokens)}</strong>
                      <span>有效样本</span><strong>{estimate.validSampleCount}/{estimate.observedSampleCount}</strong>
                      <span>有效 Token</span><strong>{formatNumber(estimate.observedTokens)}</strong>
                      <span>累计额度变化</span><strong>{estimate.observedQuotaPercent.toFixed(2)}%</strong>
                      <span>Token coverage</span><strong>{(estimate.coverageRatio * 100).toFixed(1)}%</strong>
                      <span>Pending Token</span><strong>{formatNumber(estimate.pendingTokens)}</strong>
                      <span>Boundary overlap</span><strong>{estimate.boundaryOverlapCount}（{(estimate.boundaryOverlapRatio * 100).toFixed(1)}%）</strong>
                      <span>Ambiguous boundary Token</span><strong>{formatNumber(estimate.ambiguousBoundaryTokens)}（{(estimate.ambiguousBoundaryRatio * 100).toFixed(1)}%）</strong>
                      <span>样本离散度</span><strong>{Number.isFinite(estimate.dispersionRatio) ? `${(estimate.dispersionRatio * 100).toFixed(1)}%` : "不可计算"}</strong>
                      <span>预观测 Token</span><strong>{formatNumber(estimate.preObservationTokens)}</strong>
                      <span>可归因 Token</span><strong>{formatNumber(estimate.eligibleTokens)}</strong>
                      <span>Observation gap</span><strong>{estimate.observationGapMs > 0 ? `${Math.ceil(estimate.observationGapMs / 1000)} 秒 · ${estimate.sampleQuality}` : "exact"}</strong>
                      <span>Gap 内 Token</span><strong>{formatNumber(estimate.gapTokenCount)}</strong>
                      <span>Unattributed quota</span><strong>{estimate.unattributedQuotaPercent.toFixed(2)}%</strong>
                    </div>
                    {estimate.hardBlockers.length ? (
                      <div className="quota-estimate-diagnostics-reasons">
                        Hard blockers：{estimate.hardBlockers.map(estimatorReasonLabel).join("、")}
                      </div>
                    ) : null}
                    {estimate.warnings.length ? (
                      <div className="quota-estimate-diagnostics-reasons">
                        Warnings：{estimate.warnings.map(estimatorReasonLabel).join("、")}
                      </div>
                    ) : null}
                  </details>
                ) : null}
              </div>
            );
          })}
        </div>
      )}

      <div className="quota-estimate-divider" />
      <div className="quota-estimate-remaining">
        <div>
          <h2>预计剩余可使用 Token</h2>
          <span className="muted tiny">按账号真实剩余周额度 × 已建立的分类预估模型</span>
        </div>
        <div className="quota-estimate-remaining-list">
          {categories.length ? categories.map((category) => {
            const estimate = category.weeklyEstimate;
            const remaining = estimate?.status === "estimated" && estimate.remainingTokens != null;
            return (
              <div className="quota-estimate-remaining-row" key={`${category.model}:${category.reasoningEffort}:${category.speedMode}`}>
                <span>
                  {modelLabel(category.model)} ({reasoningLabel(category.reasoningEffort)})
                  {category.fast ? <span className="fast-badge" title="快速模式" aria-label="快速模式">⚡</span> : null}
                </span>
                <strong>{remaining ? `≈ ${formatNumber(estimate.remainingTokens!)} Token` : statusText(estimate?.status ?? "insufficient_data")}</strong>
              </div>
            );
          }) : (
            <span className="muted tiny">参数过少，暂无法准确预估</span>
          )}
        </div>
      </div>
    </div>
  );
}

function TodayCodexUsageCard({
  usage,
  loading,
}: {
  usage: CategoryUsage | null;
  loading: boolean;
}) {
  const quota = usage?.quotaUsage;
  const tokenUsage = usage?.tokenUsage;
  const quotaObserved = quota?.status === "observed" && quota.value != null;
  const formatObservedQuota = quotaObserved
    ? `${quota!.value!.toFixed(2)}%`
    : usage
      ? "参数过少，暂无法准确观测"
      : "—";

  return (
    <section className="quota-card daily-model-usage-card">
      <div className="row quota-head">
        <div>
          <div className="eyebrow">使用情况 · 本地自然日</div>
          <h2>今日 Codex 使用情况</h2>
        </div>
        <div className="muted tiny">{loading ? "同步中…" : `${usage?.categories.length ?? 0} 类`}</div>
      </div>

      <div className="usage-account-metrics">
        <div className="usage-account-metric">
          <span className="eyebrow">今日消耗周额度量</span>
          <strong>{formatObservedQuota}</strong>
          <span className="muted tiny">
            {quotaObserved
              ? `已观测 · ${quota!.changeCount} 次额度变化 · ${quota!.sampleCount} 个有效采样区间`
              : usage
                ? "数据不足 · 仅接受真实额度采样"
                : "正在读取真实额度采样…"}
          </span>
        </div>
        <div className="usage-account-metric">
          <span className="eyebrow">今日消耗 Token 量</span>
          <strong>{usage ? formatNumber(tokenUsage?.valueTokens ?? 0) : "—"}</strong>
          <span className="muted tiny">
            {usage ? `已观测 · ${tokenUsage?.sampleCount ?? 0} 个本地轮次` : "正在读取本地 JSONL / 运行记录…"}
          </span>
        </div>
      </div>

      {!usage?.categories.length ? (
        <p className="muted small daily-model-usage-empty">
          {loading ? "正在读取今日分类用量…" : "今日暂无可用的本地轮次 Token 数据。"}
        </p>
      ) : (
        <div className="daily-model-usage-list">
          {usage.categories.map((category) => (
            <div
              className="daily-model-usage-row"
              key={`${category.model}:${category.reasoningEffort}:${category.speedMode}`}
            >
              <div className="daily-model-usage-label">
                <strong>
                  {category.model}
                  <span className="daily-model-usage-reasoning">({reasoningLabel(category.reasoningEffort)})</span>
                  {category.fast ? <span className="fast-badge" title="快速模式" aria-label="快速模式">⚡</span> : null}
                </strong>
                <span className="muted tiny">{category.turnCount} 个轮次</span>
              </div>
              <div className="daily-model-usage-values">
                <strong>{formatNumber(category.tokens)} Token</strong>
                <span className="muted tiny">Token 来源：本地 JSONL / 运行记录</span>
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="daily-model-usage-footnote muted tiny">
        今日额度量仅显示账号级已观测额度变化；没有将 Token 占比反推为模型额度。
      </div>
    </section>
  );
}

function HomeUsageBreakdown({
  reloadToken,
  weeklyQuota,
}: {
  reloadToken: number;
  weeklyQuota: {
    id: string;
    limitName: string;
    label: string;
    usedPercent: number;
    remainingPercent: number;
    resetsAt: number | null;
  } | null;
}) {
  const [todayUsage, setTodayUsage] = useState<CategoryUsage | null>(null);
  const [weeklyUsage, setWeeklyUsage] = useState<CategoryUsage | null>(null);
  const [loading, setLoading] = useState(false);
  const loadState = useRef({ running: false, dirty: false, mounted: true });

  const loadUsage = useCallback(async () => {
    loadState.current.dirty = true;
    if (loadState.current.running) {
      return;
    }
    loadState.current.running = true;
    while (loadState.current.dirty && loadState.current.mounted) {
      loadState.current.dirty = false;
      setLoading(true);
      try {
        const [today, weekly] = await Promise.all([
          collectorIpc.categoryUsage("day"),
          collectorIpc.categoryUsage("quota_week"),
        ]);
        if (loadState.current.mounted) {
          setTodayUsage(today);
          setWeeklyUsage(weekly);
        }
      } catch (error) {
        console.error("[UI] homepage usage load failed:", error);
      } finally {
        if (loadState.current.mounted) {
          setLoading(false);
        }
      }
    }
    loadState.current.running = false;
  }, []);

  useEffect(() => {
    loadState.current.mounted = true;
    void loadUsage();
    return () => {
      loadState.current.mounted = false;
    };
  }, [loadUsage, reloadToken]);

  return (
    <div className="quota-grid weekly-quota-row">
      {weeklyQuota ? (
        <QuotaCard
          limitName={weeklyQuota.limitName}
          title={weeklyQuota.label}
          usedPercent={weeklyQuota.usedPercent}
          remainingPercent={weeklyQuota.remainingPercent}
          resetsAt={weeklyQuota.resetsAt}
        >
          <WeeklyTokenEstimateSection usage={weeklyUsage} />
        </QuotaCard>
      ) : (
        <section className="quota-card">
          <div className="eyebrow">周额度</div>
          <h2>周额度</h2>
          <p className="muted small">未返回当前账号的周额度窗口，无法显示账号级剩余百分比。</p>
          <WeeklyTokenEstimateSection usage={weeklyUsage} />
        </section>
      )}
      <TodayCodexUsageCard usage={todayUsage} loading={loading} />
    </div>
  );
}

function CollectorHealthPanel({
  reloadToken,
  schedulerStatus,
}: {
  reloadToken: number;
  schedulerStatus: UsageSchedulerStatus | null;
}) {
  const [health, setHealth] = useState<CollectorDataHealth | null>(null);
  const [loading, setLoading] = useState(false);
  const [rebuilding, setRebuilding] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [rebuildError, setRebuildError] = useState<string | null>(null);
  const [currentAccountKey, setCurrentAccountKey] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const nextHealth = await collectorIpc.dataHealth();
      setHealth(nextHealth);
      if (nextHealth.collector.status !== "running") {
        setCurrentAccountKey(null);
      } else {
        try {
          setCurrentAccountKey((await collectorIpc.categoryUsage("day")).accountKey);
        } catch {
          setCurrentAccountKey(null);
        }
      }
      setLoadError(null);
    } catch (error) {
      setHealth(null);
      setCurrentAccountKey(null);
      setLoadError(String(error));
      console.error("[UI] collector health load failed:", error);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    let disposed = false;
    const eventNames = [
      "collector://usage-invalidated",
      "collector://rate-limit-updated",
      "collector://account-updated",
      "collector://health",
      "collector://rebuild-progress",
    ];
    const unlisten: Array<() => void> = [];
    const setupListeners = async () => {
      for (const name of eventNames) {
        const stop = await listen(name, () => {
          if (!disposed) void load();
        });
        if (disposed) {
          // `listen` resolves after cleanup in a fast unmount race. Remove
          // the just-created native listener immediately instead of waiting
          // for another render or leaking it until the next event.
          stop();
        } else {
          unlisten.push(stop);
        }
      }
    };
    void setupListeners().catch((listenerError) => {
      if (!disposed) {
        setLoadError(String(listenerError));
      }
    });
    return () => {
      disposed = true;
      while (unlisten.length > 0) {
        unlisten.pop()?.();
      }
    };
  }, [load, reloadToken]);

  useEffect(() => {
    const timer = window.setInterval(() => void load(), 5_000);
    return () => window.clearInterval(timer);
  }, [load]);

  const collector = health?.collector;
  const statusLabel = collector?.status === "running"
    ? "Collector Running"
    : collector?.status === "reconnecting"
      ? "Collector Reconnecting"
      : "Collector Unavailable";
  const unresolved = health?.unresolvedSourceCount ?? 0;
  const latestGap = health?.gaps[0];
  const accountHealth = currentAccountKey
    ? health?.accounts.find((account) => account.accountKey === currentAccountKey)
    : undefined;
  const collectorBusy = collector?.status !== "running" || schedulerStatus?.refreshing === true;
  const accountHealthSummary = useMemo(() => {
    if (!health || health.accounts.length <= 1) return null;
    const unresolvedAccounts = health.accounts.filter((account) => account.accountKey.startsWith("unresolved:"));
    const namedAccounts = health.accounts
      .filter((account) => !account.accountKey.startsWith("unresolved:"))
      .slice(0, 4)
      .map((account) => `${account.accountKey.slice(0, 28)}…=${account.status}`);
    if (unresolvedAccounts.length > 0) {
      namedAccounts.push(`${unresolvedAccounts.length} 个 unresolved source`);
    }
    return namedAccounts.join(" · ");
  }, [health]);
  const rebuild = async () => {
    if (!accountHealth) return;
    setRebuilding(true);
    try {
      await collectorIpc.rebuildAccount(accountHealth.accountKey);
      setRebuildError(null);
      await load();
    } catch (error) {
      setRebuildError(String(error));
      console.error("[UI] account rebuild failed:", error);
    } finally {
      setRebuilding(false);
    }
  };

  return (
    <section className="status-card collector-health-card" role="status" aria-live="polite" aria-atomic="true">
      <div className="row quota-head">
        <div>
          <div className="eyebrow">数据平面健康</div>
          <h2>{statusLabel}</h2>
        </div>
        <span className="muted tiny">{loading ? "读取中…" : collector?.transport ?? "IPC"}</span>
      </div>
      <div className="collector-health-grid">
        <span>Source health</span>
        <strong>{unresolved ? `${unresolved} 个 unresolved` : "无 unresolved source"}</strong>
        <span>Gap diagnostics</span>
        <strong>{latestGap ? `${latestGap.reason} · ${Math.ceil(latestGap.durationMs / 1000)} 秒` : "无已记录 gap"}</strong>
        <span>Estimator / account data</span>
        <strong>{accountHealth?.status ?? "insufficient_data"}</strong>
        <span>Latest samples</span>
        <strong>{health?.latestTokenSamples.length ?? 0} token · {health?.latestRateLimitSamples.length ?? 0} rate-limit</strong>
      </div>
      {accountHealthSummary ? (
        <p className="muted tiny collector-account-health-list">
          账号健康：{accountHealthSummary}
        </p>
      ) : null}
      {collector?.status !== "running" ? (
        <p className="quota-estimate-warning">Collector 状态与页面/Codex 连接分开显示；当前只是采集器不可用或正在重连。</p>
      ) : null}
      {unresolved > 0 ? (
        <p className="quota-estimate-warning">存在无法证明账号归属的 source，已保留数据但不会进入 estimator。</p>
      ) : null}
      {latestGap ? (
        <p className="quota-estimate-warning">最近 gap 会降低 confidence；超过 5 分钟的 long gap 会阻断对应估算。</p>
      ) : null}
      {loadError ? <p className="quota-estimate-warning">无法读取 Collector health：{loadError}</p> : null}
      {rebuildError ? <p className="quota-estimate-warning">账号重建失败：{rebuildError}</p> : null}
      {currentAccountKey && accountHealth && accountHealth.status !== "verified" ? (
        <button className="secondary-button" aria-label={`重建账号 ${accountHealth.accountKey} 数据`} onClick={() => void rebuild()} disabled={rebuilding || collectorBusy}>
          {rebuilding ? "重建中…" : collectorBusy ? "Collector 空闲后可重建" : "通过 Collector 重建账号数据"}
        </button>
      ) : null}
    </section>
  );
}

const NOTIFICATION_THRESHOLDS = [80, 90, 95, 100];

function SettingsPanel({
  settings,
  saving,
  onChange,
  schedulerStatus,
}: {
  settings: MonitorSettings;
  saving: boolean;
  onChange: (next: MonitorSettings) => void;
  schedulerStatus: UsageSchedulerStatus | null;
}) {
  const toggleThreshold = (threshold: number) => {
    const current = new Set(settings.notifyThresholds);
    if (current.has(threshold)) {
      current.delete(threshold);
    } else {
      current.add(threshold);
    }

    onChange({
      ...settings,
      notifyThresholds: [...current].sort((a, b) => a - b),
    });
  };

  return (
    <section className="settings-card">
      <div className="row settings-heading">
        <div>
          <div className="eyebrow">设置</div>
          <h2>监控与通知</h2>
        </div>
        <div className="muted tiny">
          {saving
            ? "保存中…"
            : schedulerStatus
              ? schedulerStatus.refreshing
                ? `刷新中 · 第 ${schedulerStatus.refreshGeneration ?? "—"} 代`
                : schedulerStatus.queuedRefresh
                  ? "刷新已排队"
                  : `${schedulerStatus.watcherActive ? "实时监听" : "周期校准"} · ${refreshPolicyLabel(schedulerStatus.policy)}`
              : "已保存到本机"}
        </div>
      </div>

      <div className="settings-group">
        <div className="eyebrow">通知阈值</div>
        <div className="threshold-grid">
          {NOTIFICATION_THRESHOLDS.map((threshold) => (
            <label className="check-pill" key={threshold}>
              <input
                type="checkbox"
                checked={settings.notifyThresholds.includes(threshold)}
                onChange={() => toggleThreshold(threshold)}
              />
              <span>{threshold}%</span>
            </label>
          ))}
        </div>
      </div>

      <label className="setting-row">
        <span>
          <strong>额度重置通知</strong>
          <small>额度窗口重置后通知一次，并清空该窗口的阈值去重状态。</small>
        </span>
        <input
          type="checkbox"
          checked={settings.notifyQuotaReset}
          onChange={(event) => onChange({
            ...settings,
            notifyQuotaReset: event.target.checked,
          })}
        />
      </label>

      <div className="settings-group">
        <div className="eyebrow">用量刷新</div>
        <label className="setting-row">
          <span>
            <strong>刷新策略</strong>
            <small>事件驱动采集本地轮次 Token，周期校准由 Rust 调度器负责。</small>
          </span>
          <select
            value={settings.usageRefreshPolicy === "5s" ? "adaptive" : settings.usageRefreshPolicy}
            onChange={(event) => onChange({
              ...settings,
              usageRefreshPolicy: event.target.value,
            })}
          >
            <option value="adaptive">自适应实时（实验性）</option>
            <option value="15s">每 15 秒</option>
            <option value="30s">每 30 秒</option>
            <option value="1m">每 1 分钟</option>
            <option value="3m">每 3 分钟</option>
            <option value="5m">每 5 分钟</option>
          </select>
        </label>
        <details>
          <summary>高级设置</summary>
          <label className="setting-row">
            <span>
              <strong>5 秒备用刷新</strong>
              <small>仅用于短时诊断，后台仍由 Rust Scheduler 管理。</small>
            </span>
            <select
              value={settings.usageRefreshPolicy === "5s" ? "5s" : ""}
              onChange={(event) => onChange({
                ...settings,
                usageRefreshPolicy: event.target.value || "adaptive",
              })}
            >
              <option value="">已禁用</option>
              <option value="5s">每 5 秒</option>
            </select>
          </label>
        </details>
      </div>

      <label className="setting-row">
        <span>
          <strong>开机启动</strong>
          <small>使用系统启动项启动后台监控。</small>
        </span>
        <input
          type="checkbox"
          checked={settings.launchAtStartup}
          onChange={(event) => onChange({
            ...settings,
            launchAtStartup: event.target.checked,
          })}
        />
      </label>

      <label className="setting-row">
        <span>
          <strong>启动时最小化</strong>
          <small>启动后隐藏主窗口，只保留托盘监控。</small>
        </span>
        <input
          type="checkbox"
          checked={settings.startMinimized}
          onChange={(event) => onChange({
            ...settings,
            startMinimized: event.target.checked,
          })}
        />
      </label>

      <label className="setting-row">
        <span>
          <strong>关闭窗口时隐藏到托盘</strong>
          <small>点击窗口 X 时隐藏窗口，真正退出请使用托盘菜单。</small>
        </span>
        <input
          type="checkbox"
          checked={settings.closeToTray}
          onChange={(event) => onChange({
            ...settings,
            closeToTray: event.target.checked,
          })}
        />
      </label>
    </section>
  );
}

const USAGE_RANGES: Array<{ value: UsageRange; label: string }> = [
  { value: "7d", label: "近 7 天" },
  { value: "15d", label: "近 15 天" },
  { value: "30d", label: "近 30 天" },
  { value: "90d", label: "近 90 天" },
  { value: "all", label: "全部" },
];

const USAGE_BREAKDOWNS: Array<{ value: UsageBreakdown; label: string }> = [
  { value: "model", label: "模型" },
  { value: "reasoning", label: "推理强度" },
  { value: "speed", label: "速度" },
  { value: "tokenType", label: "Token 类型" },
];

const ANALYTICS_COLORS = ["#007aff", "#5ac8fa", "#34c759", "#ff9f0a", "#af52de", "#ff375f", "#8e8e93"];

function UsageAnalyticsPanel({ reloadToken }: { reloadToken: number }) {
  const [range, setRange] = useState<UsageRange>("7d");
  const [breakdown, setBreakdown] = useState<UsageBreakdown>("model");
  const [analytics, setAnalytics] = useState<UsageAnalytics | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    void invoke<UsageAnalytics>("get_usage_analytics", { range, breakdown })
      .then((value) => {
        if (!disposed) setAnalytics(value);
      })
      .catch((error) => console.error("[UI] analytics load failed:", error))
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [range, breakdown, reloadToken]);

  const maxValue = Math.max(
    1,
    ...(analytics?.points ?? []).map((point) =>
      Object.values(point.categoryValues).reduce((sum, value) => sum + value, 0),
    ),
  );

  return (
    <section className="chart-card analytics-card">
      <div className="row analytics-heading">
        <div>
          <div className="eyebrow">实验性本地分析</div>
          <h2>本地 Codex 使用分析（实验性）</h2>
        </div>
        <div className="muted tiny">
          {loading ? "分析中…" : `${analytics?.turnCount ?? 0} 个轮次`}
        </div>
      </div>

      <div className="analytics-controls">
        <div className="segmented-control">
          {USAGE_RANGES.map((item) => (
            <button
              key={item.value}
              className={range === item.value ? "active" : ""}
              onClick={() => setRange(item.value)}
            >
              {item.label}
            </button>
          ))}
        </div>
        <label className="breakdown-select">
          <span className="muted tiny">分类维度</span>
          <select value={breakdown} onChange={(event) => setBreakdown(event.target.value as UsageBreakdown)}>
            {USAGE_BREAKDOWNS.map((item) => (
              <option key={item.value} value={item.value}>{item.label}</option>
            ))}
          </select>
        </label>
      </div>

      {analytics?.estimatedRemainingTokens != null ? (
        <div className="estimate-card">
          <div>
            <div className="eyebrow">预估剩余</div>
            <strong>≈ {formatNumber(analytics.estimatedRemainingTokens)} Token</strong>
          </div>
          <span className="muted tiny">
            根据最近 {analytics.estimateSampleCount} 个有效额度区间估算
          </span>
        </div>
      ) : null}

      {!analytics?.points.length ? (
        <p className="muted small analytics-empty">
          暂无 SQLite 分析数据。首次采集后会保留官方每日总量、本机轮次明细和无法归因的用量。
        </p>
      ) : (
        <>
          <div className="stacked-chart" aria-label="Token 活动堆叠柱状图">
            {analytics.points.map((point) => {
              const values = Object.entries(point.categoryValues);
              return (
                <div className="stacked-chart-col" key={point.date} title={`${point.date} · ${formatNumber(point.officialTokens ?? point.localTokens)} Token`}>
                  <div className="stacked-chart-value">{formatNumber(point.officialTokens ?? point.localTokens)}</div>
                  <div className="stacked-chart-track">
                    {values.map(([category, value], index) => (
                      <div
                        className="stacked-chart-segment"
                        key={category}
                        style={{
                          height: `${Math.max(1, (value / maxValue) * 100)}%`,
                          background: ANALYTICS_COLORS[index % ANALYTICS_COLORS.length],
                        }}
                        title={`${category}：${formatNumber(value)} Token`}
                      />
                    ))}
                  </div>
                  <div className="tiny muted">{point.date.slice(5)}</div>
                </div>
              );
            })}
          </div>
          <div className="analytics-legend">
            {analytics.categories.map((category, index) => (
              <span key={category}>
                <i style={{ background: ANALYTICS_COLORS[index % ANALYTICS_COLORS.length] }} />
                {category}
              </span>
            ))}
          </div>
          <div className="analytics-footnote muted tiny">
            此旧版分析仅用于本地 Token 活动与实验性派生预估，不进入上方官方额度或模型额度积分卡片。官方账号总量与本机运行记录观测保持分开；分类周额度不会在这里推算。
          </div>
        </>
      )}
    </section>
  );
}

/*
 * ============================================================
 * 日期工具
 * ============================================================
 */

function toLocalDateKey(
  date: Date,
): string {
  return [
    date.getFullYear(),

    String(
      date.getMonth() + 1,
    ).padStart(2, "0"),

    String(
      date.getDate(),
    ).padStart(2, "0"),
  ].join("-");
}

/*
 * 真正的 Today。
 *
 * 不再使用：
 *
 * 今天不存在数据
 *   ↓
 * fallback 到最后一条历史数据
 *
 * 否则例如 8/12 没数据，
 * 可能错误显示成 8/11 的 Tokens。
 */
function getTodayTokens(
  buckets:
    DailyUsageBucket[]
    | null
    | undefined,
): number | null {
  if (!buckets?.length) {
    return null;
  }

  const today =
    toLocalDateKey(
      new Date(),
    );

  return (
    buckets.find(
      (item) =>
        item.startDate === today,
    )?.tokens
    ?? null
  );
}

/*
 * API 返回的是：
 *
 * 有使用记录的日期 bucket。
 *
 * 并不一定连续。
 *
 * 所以如果 UI 写 Last 7 Days，
 * 必须主动补 0。
 */
function buildLastDays(
  buckets:
    DailyUsageBucket[]
    | null
    | undefined,

  days = 7,
): DailyUsageBucket[] {
  const map =
    new Map<string, number>(
      (buckets ?? []).map(
        (item) => [
          item.startDate,
          item.tokens,
        ],
      ),
    );

  const now =
    new Date();

  const result:
    DailyUsageBucket[] = [];

  for (
    let index = days - 1;
    index >= 0;
    index -= 1
  ) {
    const date =
      new Date(now);

    date.setHours(
      0,
      0,
      0,
      0,
    );

    date.setDate(
      now.getDate() - index,
    );

    const key =
      toLocalDateKey(date);

    result.push({
      startDate: key,
      tokens:
        map.get(key) ?? 0,
    });
  }

  return result;
}

function isFreshSignedInSnapshot(
  snapshot: CodexSnapshot | null,
  connection: CodexConnectionStatus | null,
  scheduler: UsageSchedulerStatus | null,
): snapshot is CodexSnapshot {
  const account = snapshot?.account?.account;
  const stableAccountEvidence = Boolean(
    account
      && ((typeof account.id === "string" && account.id.trim().length > 0)
        || (typeof account.accountId === "string" && account.accountId.trim().length > 0)
        || (typeof account.email === "string" && account.email.trim().length > 0)),
  );
  const rateLimitsComplete = Boolean(
    snapshot?.rateLimits
      && (snapshot.rateLimits.rateLimitsByLimitId != null
        || snapshot.rateLimits.rateLimits != null),
  );
  const usageComplete = Boolean(
    snapshot?.usage
      && snapshot.usage.dailyUsageBuckets !== undefined
      && snapshot.usage.summary !== undefined,
  );
  return Boolean(
    snapshot
      && snapshot.accountState === "signedIn"
      && stableAccountEvidence
      && !snapshot.accountError
      && rateLimitsComplete
      && usageComplete
      && !snapshot.rateLimitsError
      && !snapshot.usageError
      && connection?.phase === "ready"
      && !connection.collectorUnavailable
      && scheduler?.refreshing === false
      && snapshot.refreshGeneration != null
      && snapshot.refreshGeneration === scheduler.refreshGeneration
      && snapshot.codexGeneration != null
      && snapshot.codexGeneration === connection.generation,
  );
}

/*
 * ============================================================
 * App
 * ============================================================
 */

function App() {
  /*
   * ==========================================================
   * Snapshot
   * ==========================================================
   *
   * Codex 当前业务数据：
   *
   * - Account
   * - Rate Limits
   * - Token Usage
   */
  const [
    snapshot,
    setSnapshot,
  ] = useState<
    CodexSnapshot | null
  >(null);

  /*
   * ==========================================================
   * Connection State
   * ==========================================================
   *
   * Rust CodexRpcClient：
   *
   * disconnected
   *      ↓
   * connecting
   *      ↓
   * initializing
   *      ↓
   * ready
   *
   * 断线：
   *
   * ready
   *   ↓
   * disconnected
   *   ↓
   * reconnecting
   *   ↓
   * initializing
   *   ↓
   * ready
   */
  const [
    connection,
    setConnection,
  ] = useState<
    CodexConnectionStatus | null
  >(null);

  const [
    loading,
    setLoading,
  ] = useState(true);

  const [refreshing, setRefreshing] = useState(false);
  const pendingRefreshGeneration = useRef<number | null>(null);
  const usageInvalidationTimer = useRef<number | null>(null);

  const [
    error,
    setError,
  ] = useState<
    string | null
  >(null);

  const [
    reconnectingManually,
    setReconnectingManually,
  ] = useState(false);

  const retryCountdownMs = connection?.retryInMs ?? null;

  const [
    settings,
    setSettings,
  ] = useState<MonitorSettings | null>(null);

  const [
    analyticsReloadToken,
    setAnalyticsReloadToken,
  ] = useState(0);

  const scheduleUsageDataReload = useCallback(() => {
    if (usageInvalidationTimer.current !== null) {
      return;
    }
    usageInvalidationTimer.current = window.setTimeout(() => {
      usageInvalidationTimer.current = null;
      setAnalyticsReloadToken((value) => value + 1);
    }, 300);
  }, []);

  useEffect(() => () => {
    if (usageInvalidationTimer.current !== null) {
      window.clearTimeout(usageInvalidationTimer.current);
      usageInvalidationTimer.current = null;
    }
  }, []);

  const [schedulerStatus, setSchedulerStatus] =
    useState<UsageSchedulerStatus | null>(null);

  useEffect(() => {
    let disposed = false;
    const poll = async () => {
      try {
        const envelope = await collectorIpc.status();
        if (disposed) return;
        setSchedulerStatus(envelope.scheduler);
        if (envelope.scheduler.refreshing) {
          setSnapshot(null);
          setLoading(false);
        }
        const pending = pendingRefreshGeneration.current;
        const completed = envelope.scheduler.refreshGeneration ?? 0;
        if (envelope.scheduler.refreshError) {
          pendingRefreshGeneration.current = null;
          setSnapshot(null);
          setRefreshing(false);
          setLoading(false);
          setError(`Collector refresh failed: ${envelope.scheduler.refreshError}`);
          return;
        }
        if (pending !== null && !envelope.scheduler.refreshing && completed >= pending) {
          pendingRefreshGeneration.current = null;
          setRefreshing(false);
        }
        if (envelope.collector.status !== "running") {
          setSnapshot(null);
          setLoading(false);
          setError(`Collector ${envelope.collector.status}: health unavailable`);
        }
      } catch (error) {
        if (disposed) return;
        setSchedulerStatus(null);
        setSnapshot(null);
        pendingRefreshGeneration.current = null;
        setRefreshing(false);
        setLoading(false);
        setError(`Collector refresh status unavailable: ${String(error)}`);
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), 500);

    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    const setup = async () => {
      unlisten = await listen<UsageDataInvalidatedPayload>(
        "codex://usage-data-invalidated",
        () => {
          if (!disposed) {
            scheduleUsageDataReload();
          }
        },
      );
    };

    void setup().catch((setupError) => {
      if (!disposed) {
        console.error("[UI] usage invalidation listener failed:", setupError);
      }
    });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [scheduleUsageDataReload]);

  useEffect(() => {
    let currentDayKey = toLocalDateKey(new Date());
    let rolloverTimer: number | null = null;

    const checkLocalDay = () => {
      const nextDayKey = toLocalDateKey(new Date());
      if (nextDayKey === currentDayKey) {
        return;
      }
      currentDayKey = nextDayKey;
      scheduleUsageDataReload();
      void collectorIpc.refreshNow().catch((refreshError) => {
        console.error("[UI] local-day refresh request failed:", refreshError);
      });
    };

    const scheduleNextMidnight = () => {
      const now = new Date();
      const next = new Date(now);
      next.setHours(24, 0, 1, 0);
      rolloverTimer = window.setTimeout(() => {
        checkLocalDay();
        scheduleNextMidnight();
      }, Math.max(1000, next.getTime() - now.getTime()));
    };

    const interval = window.setInterval(checkLocalDay, 30_000);
    document.addEventListener("visibilitychange", checkLocalDay);
    window.addEventListener("focus", checkLocalDay);
    scheduleNextMidnight();

    return () => {
      window.clearInterval(interval);
      if (rolloverTimer !== null) {
        window.clearTimeout(rolloverTimer);
      }
      document.removeEventListener("visibilitychange", checkLocalDay);
      window.removeEventListener("focus", checkLocalDay);
    };
  }, [scheduleUsageDataReload]);

  const [
    settingsOpen,
    setSettingsOpen,
  ] = useState(false);

  const [
    settingsSaving,
    setSettingsSaving,
  ] = useState(false);

  useEffect(() => {
    void invoke<MonitorSettings>("get_monitor_settings").then((loadedSettings) => {
      setSettings(loadedSettings);
    }).catch((loadError) => {
      console.error("[UI] failed to load monitor data:", loadError);
    });
  }, []);

  const saveSettings = useCallback(async (next: MonitorSettings) => {
    setSettings(next);
    setSettingsSaving(true);
    try {
      const saved = await invoke<MonitorSettings>("save_monitor_settings", {
        settings: next,
      });
      setSettings(saved);
    } catch (saveError) {
      console.error("[UI] failed to save monitor settings:", saveError);
      setError(String(saveError));
    } finally {
      setSettingsSaving(false);
    }
  }, []);

  /*
   * ==========================================================
   * Snapshot Refresh
   * ==========================================================
   *
   * React 只触发一次显式刷新请求；真正的 RPC、watcher 与写入由独立
   * nexus-collector 执行，Tauri 只做代理。
   */
  const refresh =
    useCallback(
      async () => {
        try {
          setRefreshing(true);
          // Clear immediately; the collector also clears its durable latest
          // snapshot and reserves a new generation at REFRESH_NOW start.
          setSnapshot(null);
          setError(null);

          const generation = await collectorIpc.refreshNow();
          pendingRefreshGeneration.current = generation;
        } catch (error) {
          console.error(
            "[UI] snapshot refresh failed:",
            error,
          );

          setError(
            String(error),
          );
          setRefreshing(false);
        }
      },
      [],
    );

  const reconnect =
    useCallback(
      async () => {
        try {
          setReconnectingManually(true);
          setError(null);

          await invoke<void>(
            "reconnect_codex",
          );
        } catch (error) {
          console.error(
            "[UI] manual reconnect failed:",
            error,
          );

          setError(
            String(error),
          );

          setReconnectingManually(false);
        }
      },
      [],
    );

  /*
   * ==========================================================
   * Connection State Listener
   * ==========================================================
   *
   * 这部分负责：
   *
   * Collector 独立进程不依赖 Tauri event loop；GET_CODEX_STATUS 是
   * 连接状态和 retry countdown 的权威来源。
   */
  useEffect(() => {
    let disposed = false;
    const pollStatus = async () => {
      try {
        const status = await invoke<CodexConnectionStatus>("get_codex_connection_status");
        if (!disposed) {
          setConnection(status);
          if (status.collectorUnavailable || status.phase !== "ready") {
            setSnapshot(null);
            setLoading(false);
          }
        }
      } catch (error) {
        if (!disposed) {
          setConnection({
            phase: "disconnected",
            generation: 0,
            attempt: 0,
            retryInMs: null,
            lastError: String(error),
            codexPath: null,
            collectorUnavailable: true,
          });
          setSnapshot(null);
          setLoading(false);
          setError(`Collector unavailable: ${String(error)}`);
        }
      }
    };
    void pollStatus();
    const timer = window.setInterval(() => void pollStatus(), 1_000);

    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    if (
      connection?.phase
        === "ready"
      || connection?.phase
        === "disconnected"
    ) {
      setReconnectingManually(false);
    }
  }, [connection?.phase]);

  useEffect(() => {
    let disposed = false;
    let snapshotPoll: number | undefined;

    const setup = async () => {
      const cached = await invoke<CodexSnapshot | null>(
        "get_cached_codex_snapshot",
      );
      if (!disposed) {
        setSnapshot(cached ?? null);
        if (cached) {
          setAnalyticsReloadToken((value) => value + 1);
        }
        setLoading(false);
      }

      const currentSchedulerStatus = await invoke<UsageSchedulerStatus>(
        "get_usage_scheduler_status",
      );
      if (!disposed) {
        setSchedulerStatus(currentSchedulerStatus);
      }

      snapshotPoll = window.setInterval(() => {
        void invoke<CodexSnapshot | null>("get_cached_codex_snapshot")
          .then((latest) => {
            if (!disposed) {
              setSnapshot(latest ?? null);
              if (latest) {
                setAnalyticsReloadToken((value) => value + 1);
              }
              setLoading(false);
            }
          })
          .catch((error) => {
            if (!disposed) {
              setSnapshot(null);
              setLoading(false);
              setError(`Collector snapshot unavailable: ${String(error)}`);
            }
          });
        void collectorIpc.status()
          .then((envelope) => {
            if (!disposed) setSchedulerStatus(envelope.scheduler);
          })
          .catch((error) => {
            if (!disposed) {
              setSnapshot(null);
              setSchedulerStatus(null);
              setLoading(false);
              setError(`Collector health unavailable: ${String(error)}`);
            }
          });
      }, 5_000);
    };

    void setup().catch((setupError) => {
      if (!disposed) {
        console.error("[UI] usage event setup failed:", setupError);
        setLoading(false);
        setError(`Collector health unavailable: ${String(setupError)}`);
      }
    });

    return () => {
      disposed = true;
      if (snapshotPoll !== undefined) {
        window.clearInterval(snapshotPoll);
      }
    };
  }, []);

  /*
   * ==========================================================
   * Realtime Rate Limits
   * ==========================================================
   *
   * Codex：
   *
   * account/rateLimits/updated
   *
   *        ↓
   *
   * Rust：
   *
   * app.emit(
   *   "codex://rate-limits-updated"
   * )
   *
   *        ↓
   *
   * React：
   *
   * listen()
   */
  useEffect(() => {
    console.log(
      "[UI] registering rate-limit listener",
    );

    let disposed = false;

    let unlisten:
      (() => void)
      | undefined;

    const setup =
      async () => {
        unlisten =
          await listen<
            RateLimitsUpdatedPayload
          >(
            "codex://rate-limits-updated",

            (event) => {
              if (disposed) {
                return;
              }

              console.log(
                "[UI] received rate-limit update:",
                event.payload,
              );

              setSnapshot(
                (current) => {
                  /*
                   * 初始 snapshot
                   * 尚未完成时，
                   * 暂时忽略 notification。
                   *
                   * Ready 后的 refresh
                   * 会补齐完整数据。
                   */
                  if (!current) {
                    console.warn(
                      "[UI] snapshot not ready, ignoring rate-limit event",
                    );

                    return current;
                  }

                  const nextRateLimits =
                    mergeRateLimitsUpdate(
                      current.rateLimits,

                      event
                        .payload
                        .rateLimits,
                    );

                  console.log(
                    "[UI] merged rate limits:",
                    nextRateLimits,
                  );

                  return {
                    ...current,

                    rateLimits:
                      nextRateLimits,

                    fetchedAt:
                      Date.now(),
                  };
                },
              );
            },
          );

        console.log(
          "[UI] rate-limit listener ready",
        );

        if (disposed && unlisten) {
          const stop = unlisten;
          unlisten = undefined;
          stop();
        }
      };

    void setup();

    return () => {
      disposed = true;

      if (unlisten) {
        console.log(
          "[UI] removing rate-limit listener",
        );
        const stop = unlisten;
        unlisten = undefined;
        stop();
      }
    };
  }, []);

  /*
   * ==========================================================
   * Rate Limit Domain Model
   * ==========================================================
   *
   * Codex 原始数据：
   *
   * rateLimitsByLimitId
   * primary
   * secondary
   *
   *        ↓
   *
   * normalizeRateLimits()
   *
   *        ↓
   *
   * QuotaWindow[]
   *
   * 页面完全不需要知道：
   *
   * primary / secondary
   *
  * 的具体含义。
  */
  const renderableSnapshot = isFreshSignedInSnapshot(snapshot, connection, schedulerStatus)
    ? snapshot
    : null;

  const quotaWindows =
    useMemo(
      () =>
        normalizeRateLimits(
          renderableSnapshot
            ?.rateLimits
          ?? null,
        ),

      [
        renderableSnapshot
          ?.rateLimits,
      ],
    );

  const weeklyQuotaWindows = quotaWindows.filter(
    (quota) => quota.windowDurationMins === 10080,
  );
  const nonWeeklyQuotaWindows = quotaWindows.filter(
    (quota) => quota.windowDurationMins !== 10080,
  );

  /*
   * ==========================================================
   * Usage
   * ==========================================================
   */
  const usage =
    renderableSnapshot?.usage;

  const today =
    useMemo(
      () =>
        getTodayTokens(
          usage
            ?.dailyUsageBuckets,
        ),

      [
        usage
          ?.dailyUsageBuckets,
      ],
    );

  /*
   * ==========================================================
   * Connection UI
   * ==========================================================
   */

  const connectionText =
    useMemo(
      () => {
        if (!connection) {
          return "启动中";
        }

        if (connection.collectorUnavailable) {
          return "Collector 不可用";
        }

        switch (
        connection.phase
        ) {
          case "ready":
            return "已连接";

          case "connecting":
            return "连接中";

          case "initializing":
            return "初始化中";

          case "reconnecting":
            if (
              retryCountdownMs
                !== null
            ) {
              return (
                `重连中 · ${Math.ceil(
                  retryCountdownMs
                  / 1000,
                )
                } 秒后重试`
              );
            }

            return "重连中";

          case "disconnected":
            return "已断开";

          default:
            return connection.phase;
        }
      },

      [
        connection,
        retryCountdownMs,
      ],
    );

  const connectionReady =
    connection?.phase === "ready";

  const accountState:
    CodexAccountState =
    connectionReady && !connection?.collectorUnavailable
      ? snapshot?.accountState ?? "unknown"
      : "error";

  const accountText =
    useMemo(
      () => {
        switch (accountState) {
          case "signedIn":
            return "账号已登录";

          case "signedOut":
            return "账号不可用";

          case "error":
            return "账号错误";

          default:
            return "账号状态未知";
        }
      },
      [accountState],
    );

  /*
   * ==========================================================
   * Render
   * ==========================================================
   */

  return (
    <main
      className="app-shell"
    >
      {/*
       * ======================================================
       * Header
       * ======================================================
       */}

      <header
        className="topbar"
      >
        <div
          className="brand"
        >
          <div
            className="brand-mark"
          >
            <img
              className="brand-icon-body"
              src={codexIconBody}
              alt=""
              aria-hidden="true"
            />
          </div>

          <div>
            <div
              className="title"
            >
              Codex 监控
            </div>

            <div
              className="muted small"
            >
              {
                renderableSnapshot
                  ?.account
                  ?.account
                  ?.email
                ?? "本地 Codex 账号"
              }

              {
                renderableSnapshot
                  ?.account
                  ?.account
                  ?.planType

                  ? ` · ${renderableSnapshot
                    .account
                    .account
                    .planType
                  }`

                  : ""
              }
            </div>

            {/*
             * Connection status
             */}
            <div
              className={`status-line status-${connection?.phase ?? "unknown"}`}
            >
              <span
                className="status-dot"
                aria-hidden="true"
              />

              {connectionText}

              {
                connection
                  ?.generation

                  ? ` · 连接 #${connection
                    .generation
                  }`

                  : ""
              }
            </div>

            <div
              className="account-line muted tiny"
            >
              {accountText}
            </div>
          </div>
        </div>

        <div className="topbar-actions">
          <button
            className="secondary-button"
            onClick={() => setSettingsOpen((open) => !open)}
          >
            {settingsOpen ? "收起设置" : "设置"}
          </button>
          <button
            className="refresh"
            onClick={() => void refresh()}
            /* 连接没 Ready 时不能 Refresh。 */
            disabled={refreshing || !connectionReady}
          >
            {refreshing ? "刷新中…" : "刷新"}
          </button>
        </div>
      </header>

      {settingsOpen && settings ? (
        <SettingsPanel
          settings={settings}
          saving={settingsSaving}
          schedulerStatus={schedulerStatus}
          onChange={(next) => void saveSettings(next)}
        />
      ) : null}

      <CollectorHealthPanel reloadToken={analyticsReloadToken} schedulerStatus={schedulerStatus} />

      {/*
       * ======================================================
       * Connection Error
       * ======================================================
       *
       * Reconnecting 时不用把旧 Dashboard
       * 整个清掉。
       *
       * 保留最后一次成功数据，
       * 用户仍然能看到之前的额度。
       */}

      {
        connection
          && (
            connection.phase
              === "disconnected"
            || connection.phase
              === "reconnecting"
          )

          ? (
            <section
              className="error-card"
            >
              <strong>
                {connection.collectorUnavailable ? "Collector 不可用" : "Codex 连接已断开"}
              </strong>

              <p>
                {
                  connection
                    .lastError
                }
              </p>

              <p
                className="muted small"
              >
                {
                  connection.phase
                    === "reconnecting"

                    ? retryCountdownMs
                      !== null
                      && retryCountdownMs
                        > 0
                      ? `${Math.ceil(
                        retryCountdownMs
                        / 1000,
                      )} 秒后重试`
                      : "正在重试…"

                    : "等待重新连接…"
                }
              </p>

              <button
                className="refresh"
                onClick={() => {
                  void reconnect();
                }}
                disabled={
                  reconnectingManually
                }
              >
                {
                  reconnectingManually
                    ? "正在重试…"
                    : "立即重试"
                }
              </button>
            </section>
          )

          : null
      }

      {
        connectionReady
          && snapshot
          && accountState
            !== "signedIn"
          ? (
            <section
              className="error-card account-card"
            >
              <strong>
                {accountText}
              </strong>

              <p>
                {
                  accountState
                    === "signedOut"
                    ? "Codex 已连接，但当前没有已登录账号。"
                    : "Codex 已连接，但当前无法获取账号信息。"
                }
              </p>

              {
                snapshot.accountError
                  ? (
                    <p
                      className="muted small"
                    >
                      {snapshot.accountError}
                    </p>
                  )
                  : null
              }
            </section>
          )
          : null
      }

      {/*
       * ======================================================
       * Snapshot Error
       * ======================================================
       */}

      {
        error
          ? (
            <section
              className="error-card"
            >
              <strong>
                无法读取 Codex
              </strong>

              <p>
                {error}
              </p>

              <p
                className="muted small"
              >
                当前连接状态：
                {" "}
                {connectionText}
              </p>
            </section>
          )

          : null
      }

      {/*
       * ======================================================
       * Dashboard
       * ======================================================
       *
       * 断线、signed-out 或 Collector 不可用时，必须等待新鲜的
       * signed-in snapshot 才恢复额度与 usage 卡片。
       */}

      {
        renderableSnapshot
          ? (
            <>
              {/*
               * ==============================================
               * Quota
               * ==============================================
               */}

              {nonWeeklyQuotaWindows.length > 0 ? (
                <div className="quota-grid">
                  {nonWeeklyQuotaWindows.map((quota) => (
                    <QuotaCard
                      key={quota.id}
                      limitName={quota.limitName}
                      title={quota.label}
                      usedPercent={quota.usedPercent}
                      remainingPercent={quota.remainingPercent}
                      resetsAt={quota.resetsAt}
                    />
                  ))}
                </div>
              ) : null}

              <HomeUsageBreakdown
                reloadToken={analyticsReloadToken}
                weeklyQuota={weeklyQuotaWindows[0] ?? null}
              />

              <UsageAnalyticsPanel reloadToken={analyticsReloadToken} />

              {
                (renderableSnapshot.rateLimits?.rateLimitResetCredits?.availableCount ?? 0) > 0
                  ? (
                    <section className="credit-card">
                      <div className="eyebrow">已获得的重置额度</div>
                      <div className="row">
                        <h2>
                          {renderableSnapshot.rateLimits?.rateLimitResetCredits?.availableCount} 个重置额度可用
                        </h2>
                        <span className="muted small">V2 操作功能</span>
                      </div>
                      <p className="muted small">
                        当前账号有可用的 earned reset；消费动作会在后续版本接入。
                      </p>
                    </section>
                  )
                  : null
              }

              {/*
               * ==============================================
               * Token Metrics
               * ==============================================
               */}

              <section
                className="metrics-grid"
              >
                <div
                  className="metric"
                >
                  <div
                    className="eyebrow"
                  >
                    今日
                  </div>

                  <div
                    className="metric-value"
                  >
                    {
                      formatNumber(
                        today,
                      )
                    }
                  </div>

                  <div
                    className="muted small"
                  >
                    token
                  </div>
                </div>

                <div
                  className="metric"
                >
                  <div
                    className="eyebrow"
                  >
                    累计
                  </div>

                  <div
                    className="metric-value"
                  >
                    {
                      formatNumber(
                        usage
                          ?.summary
                          ?.lifetimeTokens,
                      )
                    }
                  </div>

                  <div
                    className="muted small"
                  >
                    token
                  </div>
                </div>

                <div
                  className="metric"
                >
                  <div
                    className="eyebrow"
                  >
                    单日峰值
                  </div>

                  <div
                    className="metric-value"
                  >
                    {
                      formatNumber(
                        usage
                          ?.summary
                          ?.peakDailyTokens,
                      )
                    }
                  </div>

                  <div
                    className="muted small"
                  >
                    token
                  </div>
                </div>
              </section>

            </>
          )

          : (
            !error && (
              <section
                className="status-card"
              >
                <strong>
                  {
                    connectionReady
                      ? "正在读取 Codex 数据…"
                      : "正在连接 Codex…"
                  }
                </strong>

                <p
                  className="muted small"
                >
                  {connectionText}
                </p>
              </section>
            )
          )
      }

      {/*
       * ======================================================
       * Footer
       * ======================================================
       */}

      <footer
        className="footer muted tiny"
      >
        <span>
          {
            renderableSnapshot?.codexPath
              ? `Codex: ${renderableSnapshot.codexPath
              }`

              : connection
                ?.codexPath

                ? `Codex: ${connection
                  .codexPath
                }`

                : ""
          }
        </span>

        <span>
          {
            renderableSnapshot?.fetchedAt
              ? `更新时间：${new Date(
                renderableSnapshot
                  .fetchedAt,
              )
                .toLocaleTimeString()
              }`

              : ""
          }
        </span>
      </footer>
    </main>
  );
}

export default App;

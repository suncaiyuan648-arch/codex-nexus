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
  UsageRefreshCompletedPayload,
  UsageAnalytics,
  UsageBreakdown,
  UsageRange,
  UsageSchedulerStatus,
} from "./codex-types";

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
    case "insufficient_valid_samples":
      return "有效样本不足";
    case "insufficient_observed_tokens":
      return "有效 Token 不足";
    case "insufficient_observed_quota":
      return "累计额度变化不足";
    case "coverage_below_threshold":
      return "Token coverage 不足";
    case "eligible_tokens_missing":
      return "没有可归因 Token 分母";
    case "boundary_overlap":
      return "boundary overlap 太多";
    case "mixed_category":
      return "同一额度 step 包含多个分类";
    case "no_category_token":
      return "额度 step 内没有可归因 Token";
    case "dispersion_too_high":
      return "样本离散度过高";
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
                      <span>样本离散度</span><strong>{Number.isFinite(estimate.dispersionRatio) ? `${(estimate.dispersionRatio * 100).toFixed(1)}%` : "不可计算"}</strong>
                      <span>预观测 Token</span><strong>{formatNumber(estimate.preObservationTokens)}</strong>
                      <span>可归因 Token</span><strong>{formatNumber(estimate.eligibleTokens)}</strong>
                    </div>
                    {estimate.rejectionReasons.length ? (
                      <div className="quota-estimate-diagnostics-reasons">
                        阻止原因：{estimate.rejectionReasons.map(estimatorReasonLabel).join("、")}
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
          invoke<CategoryUsage>("get_category_usage", { period: "day" }),
          invoke<CategoryUsage>("get_category_usage", { period: "quota_week" }),
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
  const lastCompletedRefreshGeneration = useRef(0);
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
    let unlisten: (() => void) | undefined;

    const setup = async () => {
      unlisten = await listen<UsageRefreshCompletedPayload>(
        "codex://usage-refresh-completed",
        (event) => {
          const payload = event.payload;
          lastCompletedRefreshGeneration.current = Math.max(
            lastCompletedRefreshGeneration.current,
            payload.refreshGeneration,
          );
          if (disposed || pendingRefreshGeneration.current !== payload.refreshGeneration) {
            return;
          }
          pendingRefreshGeneration.current = null;
          setRefreshing(false);
          if (!payload.success && payload.error) {
            setError(payload.error);
          }
        },
      );
    };

    void setup().catch((setupError) => {
      if (!disposed) {
        console.error("[UI] refresh completion listener failed:", setupError);
      }
    });

    return () => {
      disposed = true;
      unlisten?.();
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
      void invoke<number>("refresh_usage_now").catch((refreshError) => {
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
   * React 只触发一次显式刷新请求；真正的 RPC 与数据事件由 Rust
   * UsageRefreshScheduler 执行。
   */
  const refresh =
    useCallback(
      async () => {
        try {
          setRefreshing(true);
          setError(null);

          const generation = await invoke<number>("refresh_usage_now");
          pendingRefreshGeneration.current = generation;
          if (lastCompletedRefreshGeneration.current >= generation) {
            pendingRefreshGeneration.current = null;
            setRefreshing(false);
          }
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
   * Rust CodexRpcClient
   *       ↓
   * app.emit(
   *   "codex://connection-state"
   * )
   *       ↓
   * React listen()
   *
   *
   * 同时解决一个重要竞态：
   *
   * Rust 可能已经 Ready
   *      ↓
   * React Listener 还没注册
   *      ↓
   * Ready Event 丢失
   *
   * 所以：
   *
   * ① 先注册 listener
   * ② 再主动读取当前状态
   */
  useEffect(() => {
    let disposed = false;

    let unlisten:
      (() => void)
      | undefined;

    const setupConnectionListener =
      async () => {
        /*
         * --------------------------------------
         * 1. 先监听未来状态
         * --------------------------------------
         */
        unlisten =
          await listen<
            CodexConnectionStatus
          >(
            "codex://connection-state",

            (event) => {
              if (disposed) {
                return;
              }

              console.log(
                "[UI] connection state:",
                event.payload,
              );

              setConnection(
                event.payload,
              );

            },
          );

        if (disposed) {
          unlisten();
          return;
        }

        /*
         * --------------------------------------
         * 2. 再读取当前状态
         * --------------------------------------
         *
         * 避免 Ready Event 在 React
         * listener 注册前已经发生。
         */
        try {
          const status =
            await invoke<
              CodexConnectionStatus
            >(
              "get_codex_connection_status",
            );

          if (disposed) {
            return;
          }

          console.log(
            "[UI] initial connection state:",
            status,
          );

          setConnection(
            status,
          );

          const cached = await invoke<CodexSnapshot | null>(
            "get_cached_codex_snapshot",
          );
          if (cached && !disposed) {
            setSnapshot(cached);
            setAnalyticsReloadToken((value) => value + 1);
            setLoading(false);
          }
        } catch (error) {
          console.error(
            "[UI] failed to read connection status:",
            error,
          );
        }
      };

    void setupConnectionListener();

    return () => {
      disposed = true;

      if (unlisten) {
        unlisten();
      }
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
    let unlistenSnapshot: (() => void) | undefined;
    let unlistenScheduler: (() => void) | undefined;

    const setup = async () => {
      unlistenSnapshot = await listen<CodexSnapshot>(
        "codex://usage-snapshot",
        (event) => {
          if (disposed) {
            return;
          }
          setSnapshot(event.payload);
          setAnalyticsReloadToken((value) => value + 1);
          setLoading(false);
          setError(null);
        },
      );

      unlistenScheduler = await listen<UsageSchedulerStatus>(
        "codex://usage-refresh-state",
        (event) => {
          if (!disposed) {
            setSchedulerStatus(event.payload);
          }
        },
      );

      const cached = await invoke<CodexSnapshot | null>(
        "get_cached_codex_snapshot",
      );
      if (cached && !disposed) {
        setSnapshot(cached);
        setAnalyticsReloadToken((value) => value + 1);
        setLoading(false);
      }

      const currentSchedulerStatus = await invoke<UsageSchedulerStatus>(
        "get_usage_scheduler_status",
      );
      if (!disposed) {
        setSchedulerStatus(currentSchedulerStatus);
      }
    };

    void setup().catch((setupError) => {
      if (!disposed) {
        console.error("[UI] usage event setup failed:", setupError);
      }
    });

    return () => {
      disposed = true;
      unlistenSnapshot?.();
      unlistenScheduler?.();
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

        if (
          disposed
          && unlisten
        ) {
          unlisten();
        }
      };

    void setup();

    return () => {
      disposed = true;

      if (unlisten) {
        console.log(
          "[UI] removing rate-limit listener",
        );

        unlisten();
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
  const quotaWindows =
    useMemo(
      () =>
        normalizeRateLimits(
          snapshot
            ?.rateLimits
          ?? null,
        ),

      [
        snapshot
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
    snapshot?.usage;

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
    snapshot?.accountState
    ?? "unknown";

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
                snapshot
                  ?.account
                  ?.account
                  ?.email
                ?? "本地 Codex 账号"
              }

              {
                snapshot
                  ?.account
                  ?.account
                  ?.planType

                  ? ` · ${snapshot
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
                Codex 连接已断开
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
       * 即使断线，
       * snapshot 仍然保留最后一次成功数据。
       */}

      {
        snapshot
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
                (snapshot.rateLimits?.rateLimitResetCredits?.availableCount ?? 0) > 0
                  ? (
                    <section className="credit-card">
                      <div className="eyebrow">已获得的重置额度</div>
                      <div className="row">
                        <h2>
                          {snapshot.rateLimits?.rateLimitResetCredits?.availableCount} 个重置额度可用
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
            snapshot?.codexPath
              ? `Codex: ${snapshot.codexPath
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
            snapshot?.fetchedAt
              ? `更新时间：${new Date(
                snapshot
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

import {
  useCallback,
  useEffect,
  useMemo,
  useState,
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
  DailyModelUsage,
  DailyUsageBucket,
  MonitorSettings,
  RateLimitsUpdatedPayload,
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
}: {
  limitName: string;
  title: string;
  usedPercent: number;
  remainingPercent: number;
  resetsAt: number | null;
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
    </section>
  );
}

function DailyModelUsageCard({ reloadToken }: { reloadToken: number }) {
  const [usage, setUsage] = useState<DailyModelUsage | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    void invoke<DailyModelUsage>("get_daily_model_usage")
      .then((value) => {
        if (!disposed) {
          setUsage(value);
        }
      })
      .catch((error) => console.error("[UI] daily model usage load failed:", error))
      .finally(() => {
        if (!disposed) {
          setLoading(false);
        }
      });
    return () => {
      disposed = true;
    };
  }, [reloadToken]);

  return (
    <section className="quota-card daily-model-usage-card">
      <div className="row quota-head">
        <div>
          <div className="eyebrow">Today · Local Turn Token</div>
          <h2>当日消耗额度 token 用量（周额度消耗量）</h2>
        </div>
        <div className="muted tiny">
          {loading ? "采集中…" : `${usage?.categories.length ?? 0} 类`}
        </div>
      </div>

      <div className="daily-model-usage-notice">
        <strong>周额度按账号聚合</strong>
        <span>
          Codex 当前接口没有返回模型级周额度消耗，以下 Token 为本地逐 Turn 的真实记录；不对模型强行分摊额度。
        </span>
      </div>

      {!usage?.categories.length ? (
        <p className="muted small daily-model-usage-empty">
          今日暂无可用的本地 Turn Token 数据。
        </p>
      ) : (
        <div className="daily-model-usage-list">
          {usage.categories.map((category) => {
            const isFast = category.speedMode === "fast_requested";
            return (
              <div
                className="daily-model-usage-row"
                key={`${category.model}:${category.reasoningEffort}:${category.speedMode}`}
              >
                <div className="daily-model-usage-label">
                  <strong>
                    {category.model}
                    <span className="daily-model-usage-reasoning">
                      ({category.reasoningEffort})
                    </span>
                    {isFast ? (
                      <span className="fast-badge" title="Fast mode" aria-label="Fast mode">⚡</span>
                    ) : null}
                  </strong>
                  <span className="muted tiny">{category.turnCount} 个 Turn</span>
                </div>
                <div className="daily-model-usage-values">
                  <strong>{formatNumber(category.rawTokens)} tokens</strong>
                  <span className="muted tiny">周额度：无法精确归因</span>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {usage?.officialTokens != null ? (
        <div className="daily-model-usage-footnote muted tiny">
          官方账号当日总量：{formatNumber(usage.officialTokens)} tokens；它不包含模型级额度分配。
        </div>
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
              ? `${schedulerStatus.watcherActive ? "实时监听" : "周期校准"} · ${schedulerStatus.policy}`
              : "已保存到本机"}
        </div>
      </div>

      <div className="settings-group">
        <div className="eyebrow">Notify me at</div>
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
          <strong>Quota reset notifications</strong>
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
        <div className="eyebrow">Usage refresh</div>
        <label className="setting-row">
          <span>
            <strong>Refresh policy</strong>
            <small>事件驱动采集本地 Turn Token，周期校准由 Rust 调度器负责。</small>
          </span>
          <select
            value={settings.usageRefreshPolicy === "5s" ? "adaptive" : settings.usageRefreshPolicy}
            onChange={(event) => onChange({
              ...settings,
              usageRefreshPolicy: event.target.value,
            })}
          >
            <option value="adaptive">Adaptive Realtime (Experimental)</option>
            <option value="15s">15s</option>
            <option value="30s">30s</option>
            <option value="1m">1m</option>
            <option value="3m">3m</option>
            <option value="5m">5m</option>
          </select>
        </label>
        <details>
          <summary>Advanced</summary>
          <label className="setting-row">
            <span>
              <strong>5s fallback</strong>
              <small>仅用于短时诊断，后台仍由 Rust Scheduler 管理。</small>
            </span>
            <select
              value={settings.usageRefreshPolicy === "5s" ? "5s" : ""}
              onChange={(event) => onChange({
                ...settings,
                usageRefreshPolicy: event.target.value || "adaptive",
              })}
            >
              <option value="">Disabled</option>
              <option value="5s">5s</option>
            </select>
          </label>
        </details>
      </div>

      <label className="setting-row">
        <span>
          <strong>Launch at startup</strong>
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
          <strong>Start minimized</strong>
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
          <strong>Close window to tray</strong>
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
  { value: "7d", label: "7D" },
  { value: "15d", label: "15D" },
  { value: "30d", label: "30D" },
  { value: "90d", label: "90D" },
  { value: "all", label: "ALL" },
];

const USAGE_BREAKDOWNS: Array<{ value: UsageBreakdown; label: string }> = [
  { value: "model", label: "Model" },
  { value: "reasoning", label: "Reasoning" },
  { value: "speed", label: "Speed" },
  { value: "tokenType", label: "Token type" },
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
          <div className="eyebrow">Token Activity</div>
          <h2>本地 Codex 使用分析</h2>
        </div>
        <div className="muted tiny">
          {loading ? "分析中…" : `${analytics?.turnCount ?? 0} 个 Turn`}
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
          <span className="muted tiny">Breakdown by</span>
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
            <div className="eyebrow">Estimated Remaining</div>
            <strong>≈ {formatNumber(analytics.estimatedRemainingTokens)} tokens</strong>
          </div>
          <span className="muted tiny">
            根据最近 {analytics.estimateSampleCount} 个有效额度区间估算
          </span>
        </div>
      ) : null}

      {!analytics?.points.length ? (
        <p className="muted small analytics-empty">
          暂无 SQLite 分析数据。首次采集后会保留官方每日总量、本机 Turn 明细和无法归因的用量。
        </p>
      ) : (
        <>
          <div className="stacked-chart" aria-label="Token activity stacked bar chart">
            {analytics.points.map((point) => {
              const values = Object.entries(point.categoryValues);
              return (
                <div className="stacked-chart-col" key={point.date} title={`${point.date} · ${formatNumber(point.officialTokens ?? point.localTokens)} token`}>
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
                        title={`${category}: ${formatNumber(value)}`}
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
            Official total is the account-level source. Local rollout data is only used for attribution; missing portions are shown as Unattributed. Any category quota values here are derived estimates, not official model-level billing. Cached input and reasoning are telemetry categories, not extra billing totals.
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

  const [schedulerStatus, setSchedulerStatus] =
    useState<UsageSchedulerStatus | null>(null);

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
          setLoading(true);
          setError(null);

          await invoke<void>("refresh_usage_now");
        } catch (error) {
          console.error(
            "[UI] snapshot refresh failed:",
            error,
          );

          setError(
            String(error),
          );
        } finally {
          setLoading(false);
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
            disabled={loading || !connectionReady}
          >
            {loading ? "加载中…" : "刷新"}
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

              {weeklyQuotaWindows.length > 0 ? (
                <div className="quota-grid weekly-quota-row">
                  {weeklyQuotaWindows.map((quota) => (
                    <QuotaCard
                      key={quota.id}
                      limitName={quota.limitName}
                      title={quota.label}
                      usedPercent={quota.usedPercent}
                      remainingPercent={quota.remainingPercent}
                      resetsAt={quota.resetsAt}
                    />
                  ))}
                  <DailyModelUsageCard reloadToken={analyticsReloadToken} />
                </div>
              ) : (
                !loading && (
                  <section className="error-card">
                    <strong>未返回速率限制数据。</strong>
                  </section>
                )
              )}

              <UsageAnalyticsPanel reloadToken={analyticsReloadToken} />

              {
                (snapshot.rateLimits?.rateLimitResetCredits?.availableCount ?? 0) > 0
                  ? (
                    <section className="credit-card">
                      <div className="eyebrow">Earned reset credits</div>
                      <div className="row">
                        <h2>
                          {snapshot.rateLimits?.rateLimitResetCredits?.availableCount} reset available
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

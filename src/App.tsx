import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  useRef,
} from "react";

import {
  invoke,
} from "@tauri-apps/api/core";

import {
  listen,
} from "@tauri-apps/api/event";

import type {
  CodexConnectionStatus,
  CodexSnapshot,
  DailyUsageBucket,
  RateLimitsUpdatedPayload,
} from "./codex-types";

import {
  mergeRateLimitsUpdate,
  normalizeRateLimits,
} from "./codex-normalize";

import {
  formatNumber,
  formatReset,
} from "./lib/format";

/*
 * ============================================================
 * Quota Card
 * ============================================================
 */
function QuotaCard({
  title,
  usedPercent,
  remainingPercent,
  resetsAt,
}: {
  title: string;
  usedPercent: number;
  remainingPercent: number;
  resetsAt: number;
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
            RATE LIMIT
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
        aria-label={`${used}% used`}
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
          remaining
        </span>

        <span>
          {formatReset(resetsAt)}
        </span>
      </div>
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

  /*
   * 防止同一个 generation
   * 重复触发 refresh。
   *
   * generation 1 Ready
   *      ↓
   * refresh()
   *
   * generation 1 Ready 再次通知
   *      ↓
   * 不 refresh
   *
   * generation 2 Ready
   *      ↓
   * refresh()
   */
  const lastReadyGeneration =
    useRef<number | null>(null);

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

  /*
   * ==========================================================
   * Snapshot Refresh
   * ==========================================================
   *
   * 注意：
   *
   * 现在这个 invoke 已经不会 spawn 新 app-server。
   *
   * React
   *    ↓
   * get_codex_snapshot
   *    ↓
   * Singleton CodexRpcClient
   *    ↓
   * 同一个 app-server
   */
  const refresh =
    useCallback(
      async () => {
        try {
          setLoading(true);
          setError(null);

          const data =
            await invoke<
              CodexSnapshot
            >(
              "get_codex_snapshot",
            );

          setSnapshot(data);
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

    /*
     * 每个 generation
     * 只触发一次 snapshot refresh。
     */
    const refreshWhenReady = (
      status:
        CodexConnectionStatus,
    ) => {
      if (
        status.phase !== "ready"
      ) {
        return;
      }

      if (
        lastReadyGeneration.current
        === status.generation
      ) {
        return;
      }

      console.log(
        "[UI] Codex ready, generation:",
        status.generation,
      );

      lastReadyGeneration.current =
        status.generation;

      /*
       * 新连接 Ready 后重新读取：
       *
       * Account
       * Rate Limits
       * Usage
       *
       * 这样即使断线期间数据变化，
       * 恢复连接后也会自动同步。
       */
      void refresh();
    };

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

              refreshWhenReady(
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

          refreshWhenReady(
            status,
          );
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
  }, [refresh]);

  /*
   * ==========================================================
   * Token Usage Polling
   * ==========================================================
   *
   * 注意：
   *
   * 这里已经不再：
   *
   * useEffect(() => {
   *   void refresh();
   * }, [])
   *
   * 初次 refresh 由：
   *
   * connection → ready
   *
   * 自动触发。
   *
   *
   * Ready：
   *
   * 每 5 分钟刷新一次 Token Usage。
   *
   * Disconnected / Reconnecting：
   *
   * 不做 RPC polling。
   */
  useEffect(() => {
    if (
      connection?.phase
      !== "ready"
    ) {
      return;
    }

    const timer =
      window.setInterval(
        () => {
          void refresh();
        },

        5 * 60 * 1000,
      );

    return () => {
      window.clearInterval(
        timer,
      );
    };
  }, [
    refresh,
    connection?.phase,
  ]);

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
   * 真正的最近 7 个自然日。
   *
   * API 没有 bucket 的日期：
   *
   * tokens = 0
   */
  const buckets =
    useMemo(
      () =>
        buildLastDays(
          usage
            ?.dailyUsageBuckets,

          7,
        ),

      [
        usage
          ?.dailyUsageBuckets,
      ],
    );

  const maxTokens =
    useMemo(
      () =>
        Math.max(
          1,

          ...buckets.map(
            (item) =>
              item.tokens,
          ),
        ),

      [buckets],
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
          return "Starting";
        }

        switch (
        connection.phase
        ) {
          case "ready":
            return "Connected";

          case "connecting":
            return "Connecting";

          case "initializing":
            return "Initializing";

          case "reconnecting":
            if (
              connection.retryInMs
            ) {
              return (
                `Reconnecting · retry in ${Math.ceil(
                  connection
                    .retryInMs
                  / 1000,
                )
                }s`
              );
            }

            return "Reconnecting";

          case "disconnected":
            return "Disconnected";

          default:
            return connection.phase;
        }
      },

      [connection],
    );

  const connectionReady =
    connection?.phase === "ready";

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
            className="brand-dot"
          />

          <div>
            <div
              className="title"
            >
              Codex Monitor
            </div>

            <div
              className="muted small"
            >
              {
                snapshot
                  ?.account
                  ?.account
                  ?.email
                ?? "Local Codex account"
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
              className="muted tiny"
            >
              {connectionText}

              {
                connection
                  ?.generation

                  ? ` · connection #${connection
                    .generation
                  }`

                  : ""
              }
            </div>
          </div>
        </div>

        <button
          className="refresh"

          onClick={
            () =>
              void refresh()
          }

          /*
           * 连接没 Ready 时不能 Refresh。
           */
          disabled={
            loading
            || !connectionReady
          }
        >
          {
            loading
              ? "Loading…"
              : "Refresh"
          }
        </button>
      </header>

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
          ?.lastError
          && connection.phase
          !== "ready"

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

                    ? "正在自动重新连接 Codex…"

                    : "等待重新连接 Codex…"
                }
              </p>
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

              {
                quotaWindows.length
                  > 0
                  ? (
                    <div
                      className="quota-grid"
                    >
                      {
                        quotaWindows.map(
                          (quota) => (
                            <QuotaCard
                              key={
                                quota.id
                              }

                              title={
                                quota.label
                              }

                              usedPercent={
                                quota
                                  .usedPercent
                              }

                              remainingPercent={
                                quota
                                  .remainingPercent
                              }

                              resetsAt={
                                quota
                                  .resetsAt
                              }
                            />
                          ),
                        )
                      }
                    </div>
                  )

                  : (
                    !loading && (
                      <section
                        className="error-card"
                      >
                        <strong>
                          No rate limit
                          data returned.
                        </strong>
                      </section>
                    )
                  )
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
                    TODAY
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
                    tokens
                  </div>
                </div>

                <div
                  className="metric"
                >
                  <div
                    className="eyebrow"
                  >
                    LIFETIME
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
                    tokens
                  </div>
                </div>

                <div
                  className="metric"
                >
                  <div
                    className="eyebrow"
                  >
                    PEAK DAY
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
                    tokens
                  </div>
                </div>
              </section>

              {/*
               * ==============================================
               * Token Chart
               * ==============================================
               */}

              <section
                className="chart-card"
              >
                <div
                  className="row"
                >
                  <div>
                    <div
                      className="eyebrow"
                    >
                      ACTIVITY
                    </div>

                    <h2>
                      Last 7 days
                    </h2>
                  </div>

                  <div
                    className="muted tiny"
                  >
                    {
                      connectionReady
                        ? "Quota realtime · Usage 5 min"

                        : "Offline · showing last known data"
                    }
                  </div>
                </div>

                <div
                  className="chart"
                >
                  {
                    buckets.map(
                      (item) => {
                        const height =
                          item.tokens === 0
                            ? 0

                            : Math.max(
                              6,

                              (
                                item.tokens
                                / maxTokens
                              )
                              * 100,
                            );

                        return (
                          <div
                            className="chart-col"

                            key={
                              item.startDate
                            }

                            title={
                              `${item.startDate
                              }: ${item.tokens
                              } tokens`
                            }
                          >
                            <div
                              className="chart-value"
                            >
                              {
                                formatNumber(
                                  item.tokens,
                                )
                              }
                            </div>

                            <div
                              className="chart-track"
                            >
                              <div
                                className="chart-bar"

                                style={{
                                  height:
                                    `${height}%`,
                                }}
                              />
                            </div>

                            <div
                              className="tiny muted"
                            >
                              {
                                item
                                  .startDate
                                  .slice(5)
                              }
                            </div>
                          </div>
                        );
                      },
                    )
                  }
                </div>
              </section>
            </>
          )

          : (
            !error && (
              <section
                className="error-card"
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
              ? `Updated ${new Date(
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
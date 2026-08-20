import { invoke } from "@tauri-apps/api/core";

import type {
  CategoryUsage,
  CollectorDataHealth,
  CollectorStatusEnvelope,
} from "./codex-types";

/**
 * UI-facing proxy. React talks to these commands, never to the collector
 * scheduler or the usage database. The Tauri commands are transport adapters
 * for the local collector IPC endpoint; the independent collector owns the
 * scheduler, app-server RPC and durable writer.
 */
export const collectorIpc = {
  status: () => invoke<CollectorStatusEnvelope>("get_collector_status"),
  refreshNow: () => invoke<number>("refresh_usage_now"),
  account: () => invoke<unknown>("get_collector_account"),
  categoryUsage: (period: "day" | "quota_week") =>
    invoke<CategoryUsage>("get_category_usage", { period }),
  dataHealth: () => invoke<CollectorDataHealth>("get_collector_data_health"),
  rebuildAccount: (accountKey: string) =>
    invoke<boolean>("rebuild_collector_account", { accountKey }),
};

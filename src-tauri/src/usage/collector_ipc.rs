//! Collector/UI protocol boundary.
//!
//! The wire format is newline-delimited JSON implemented by the standalone
//! `nexus-collector` process without a dependency on Tauri. Unix uses a local
//! domain socket; Windows keeps the
//! transport behind this module so Named Pipes can replace it without changing
//! commands or the TypeScript client.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Wry};

use super::{db, models::AccountDataHealth, rollout, scheduler::UsageSchedulerStatus};

pub const IPC_SOCKET_FILE: &str = "collector.sock";
pub const EVENT_USAGE_INVALIDATED: &str = "collector://usage-invalidated";
pub const EVENT_RATE_LIMIT_UPDATED: &str = "collector://rate-limit-updated";
pub const EVENT_ACCOUNT_UPDATED: &str = "collector://account-updated";
pub const EVENT_HEALTH: &str = "collector://health";
pub const EVENT_REBUILD_PROGRESS: &str = "collector://rebuild-progress";
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_CONNECTIONS: usize = 32;
pub const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CollectorMethod {
    GetStatus,
    RefreshNow,
    GetAccount,
    GetCategoryUsage,
    GetDataHealth,
    RebuildAccount,
    GetSnapshot,
    GetCodexStatus,
    ReconnectCodex,
}

impl CollectorMethod {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "GET_STATUS" => Ok(Self::GetStatus),
            "REFRESH_NOW" => Ok(Self::RefreshNow),
            "GET_ACCOUNT" => Ok(Self::GetAccount),
            "GET_CATEGORY_USAGE" => Ok(Self::GetCategoryUsage),
            "GET_DATA_HEALTH" => Ok(Self::GetDataHealth),
            "REBUILD_ACCOUNT" => Ok(Self::RebuildAccount),
            "GET_SNAPSHOT" => Ok(Self::GetSnapshot),
            "GET_CODEX_STATUS" => Ok(Self::GetCodexStatus),
            "RECONNECT_CODEX" => Ok(Self::ReconnectCodex),
            other => Err(format!("unsupported collector method: {other}")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectorStatus {
    pub status: String,
    pub instance_id: Option<String>,
    pub pid: Option<i64>,
    pub started_at: Option<i64>,
    pub heartbeat_at: Option<i64>,
    pub heartbeat_age_ms: Option<i64>,
    pub version: Option<String>,
    pub transport: String,
    pub endpoint: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceHealth {
    pub source_id: String,
    pub path: String,
    pub account_key: Option<String>,
    pub binding_status: String,
    pub binding_source: String,
    pub health_status: String,
    pub last_offset: i64,
    pub last_size: i64,
    pub last_activity_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GapDiagnostic {
    pub start_at: i64,
    pub end_at: i64,
    pub duration_ms: i64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSample {
    pub account_key: String,
    pub sampled_at: i64,
    pub limit_id: String,
    pub window: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenSample {
    pub account_key: String,
    pub thread_id: String,
    pub turn_id: String,
    pub sampled_at: i64,
    pub delta_tokens: i64,
    pub cumulative_tokens: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectorDataHealth {
    pub collector: CollectorStatus,
    pub sources: Vec<SourceHealth>,
    pub unresolved_source_count: i64,
    pub gaps: Vec<GapDiagnostic>,
    pub latest_rate_limit_samples: Vec<RateLimitSample>,
    pub latest_token_samples: Vec<TokenSample>,
    pub accounts: Vec<AccountDataHealth>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectorStatusEnvelope {
    pub collector: CollectorStatus,
    pub scheduler: UsageSchedulerStatus,
}

pub fn endpoint_path(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    Ok(db::database_path(app)?.with_file_name(IPC_SOCKET_FILE))
}

#[cfg(unix)]
pub fn validate_endpoint(endpoint: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(endpoint)
        .map_err(|error| format!("collector endpoint metadata: {error}"))?;
    if !metadata.file_type().is_socket() {
        return Err("collector endpoint is not a Unix socket".into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("collector endpoint owner does not match current user".into());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("collector endpoint permissions must be 0600".into());
    }
    Ok(())
}

#[cfg(unix)]
pub fn bind_secure_listener(
    endpoint: &std::path::Path,
) -> Result<std::os::unix::net::UnixListener, String> {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        os::unix::net::{UnixListener, UnixStream},
    };
    if let Some(parent) = endpoint.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let listener = match UnixListener::bind(endpoint) {
        Ok(listener) => listener,
        Err(error) if endpoint.exists() => {
            // Never unlink an endpoint owned by another user or one that is
            // not already a locked-down socket. This prevents a typo or a
            // compromised path from turning stale cleanup into file deletion.
            validate_endpoint(endpoint)?;
            if UnixStream::connect(endpoint).is_ok() {
                return Err(format!(
                    "collector IPC endpoint is already active: {}",
                    endpoint.display()
                ));
            }
            fs::remove_file(endpoint).map_err(|remove_error| {
                format!("stale collector endpoint removal failed: {remove_error}")
            })?;
            UnixListener::bind(endpoint).map_err(|bind_error| {
                format!("collector IPC bind {endpoint:?}: {error}; retry failed: {bind_error}")
            })?
        }
        Err(error) => return Err(format!("collector IPC bind {endpoint:?}: {error}")),
    };
    fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    validate_endpoint(endpoint)?;
    Ok(listener)
}

pub fn read_limited_line<R: Read>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(256);
    let mut one = [0_u8; 1];
    loop {
        match reader.read(&mut one) {
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) => {
                if bytes.len() >= MAX_REQUEST_BYTES {
                    return Err("collector IPC request exceeds size limit".into());
                }
                bytes.push(one[0]);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    if bytes.is_empty() {
        return Err("collector IPC request is empty".into());
    }
    Ok(bytes)
}

pub fn emit_event(app: &AppHandle<Wry>, event: &str, payload: impl Serialize + Clone) {
    let _ = app.emit(event, payload);
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

pub fn status_from_connection(
    connection: Option<&rusqlite::Connection>,
    endpoint: &str,
) -> CollectorStatus {
    let state = connection.and_then(|connection| {
        connection
            .query_row(
                "SELECT collector_instance_id, pid, started_at, heartbeat_at, version, status
                 FROM collector_state WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten()
    });
    let heartbeat_age_ms = state
        .as_ref()
        .map(|(_, _, _, heartbeat, _, _)| (now_ms() - heartbeat.saturating_mul(1000)).max(0));
    let status = match state.as_ref() {
        None => "unavailable",
        Some((_, _, _, _, _, state)) if state != "running" => "reconnecting",
        Some(_) if heartbeat_age_ms.is_some_and(|age| age > 15_000) => "reconnecting",
        Some(_) => "running",
    };
    CollectorStatus {
        status: status.into(),
        instance_id: state.as_ref().map(|value| value.0.clone()),
        pid: state.as_ref().map(|value| value.1),
        started_at: state.as_ref().map(|value| value.2),
        heartbeat_at: state.as_ref().map(|value| value.3),
        heartbeat_age_ms,
        version: state.as_ref().map(|value| value.4.clone()),
        transport: if cfg!(unix) {
            "unix_socket"
        } else {
            "named_pipe"
        }
        .into(),
        endpoint: endpoint.into(),
    }
}

pub fn data_health(app: &AppHandle<Wry>) -> Result<CollectorDataHealth, String> {
    let endpoint = endpoint_path(app)?;
    let connection = db::open_database(app)?;
    data_health_for_connection(&connection, &endpoint.to_string_lossy())
}

pub fn data_health_for_connection(
    connection: &rusqlite::Connection,
    endpoint: &str,
) -> Result<CollectorDataHealth, String> {
    let collector = status_from_connection(Some(connection), endpoint);
    let sources = connection
        .prepare(
            "SELECT source_id, canonical_path, account_key, binding_status,
                    binding_source, health_status, last_offset, last_size,
                    last_activity_at, last_error
             FROM rollout_sources ORDER BY updated_at DESC",
        )
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok(SourceHealth {
                source_id: row.get(0)?,
                path: row.get(1)?,
                account_key: row.get(2)?,
                binding_status: row.get(3)?,
                binding_source: row.get(4)?,
                health_status: row.get(5)?,
                last_offset: row.get(6)?,
                last_size: row.get(7)?,
                last_activity_at: row.get(8)?,
                last_error: row.get(9)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let gaps = connection
        .prepare(
            "SELECT start_at, end_at, duration_ms, reason FROM collector_gaps
             ORDER BY end_at DESC LIMIT 20",
        )
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok(GapDiagnostic {
                start_at: row.get(0)?,
                end_at: row.get(1)?,
                duration_ms: row.get(2)?,
                reason: row.get(3)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let latest_rate_limit_samples = connection
        .prepare(
            "SELECT account_key, sampled_at, limit_id, window, used_percent, resets_at
             FROM rate_limit_samples ORDER BY sampled_at DESC, id DESC LIMIT 20",
        )
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok(RateLimitSample {
                account_key: row.get(0)?,
                sampled_at: row.get(1)?,
                limit_id: row.get(2)?,
                window: row.get(3)?,
                used_percent: row.get(4)?,
                resets_at: row.get(5)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let latest_token_samples = connection
        .prepare(
            "SELECT account_key, thread_id, turn_id, sampled_at, delta_tokens, cumulative_tokens
             FROM turn_token_samples ORDER BY sampled_at DESC, id DESC LIMIT 20",
        )
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok(TokenSample {
                account_key: row.get(0)?,
                thread_id: row.get(1)?,
                turn_id: row.get(2)?,
                sampled_at: row.get(3)?,
                delta_tokens: row.get(4)?,
                cumulative_tokens: row.get(5)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut account_rows = connection
        .prepare("SELECT account_key FROM account_usage_data_versions ORDER BY account_key")
        .map_err(|error| error.to_string())?;
    let accounts = account_rows
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .map(|row| row.map_err(|error| error.to_string()))
        .filter_map(|row| row.ok())
        .map(|account| rollout::account_data_health(&connection, &account))
        .filter_map(|row| row.transpose())
        .collect::<Result<Vec<_>, _>>()?;
    let unresolved_source_count = sources
        .iter()
        .filter(|source| source.binding_status == "unresolved")
        .count() as i64;
    Ok(CollectorDataHealth {
        collector,
        sources,
        unresolved_source_count,
        gaps,
        latest_rate_limit_samples,
        latest_token_samples,
        accounts,
    })
}

pub fn request_path(endpoint: &PathBuf, method: &str, params: Value) -> Result<Value, String> {
    let request = IpcRequest {
        id: format!("ui-{}", now_ms()),
        method: method.into(),
        params,
    };
    #[cfg(unix)]
    validate_endpoint(endpoint)?;
    #[cfg(unix)]
    let mut stream = std::os::unix::net::UnixStream::connect(endpoint)
        .map_err(|error| format!("collector unavailable: {error}"))?;
    #[cfg(not(unix))]
    return Err(format!(
        "collector Named Pipe transport is not installed ({})",
        endpoint.display()
    ));
    #[cfg(unix)]
    {
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|error| error.to_string())?;
        let encoded = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err("collector IPC request exceeds size limit".into());
        }
        stream
            .write_all(&encoded)
            .map_err(|error| error.to_string())?;
        stream.write_all(b"\n").map_err(|error| error.to_string())?;
        let response = read_limited_line(&mut stream)?;
        let response: IpcResponse =
            serde_json::from_slice(&response).map_err(|error| error.to_string())?;
        if response.ok {
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "collector request failed".into()))
        }
    }
}

pub fn write_response(stream: &mut impl Write, response: &IpcResponse) -> Result<(), String> {
    let encoded = serde_json::to_vec(response).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err("collector IPC response exceeds size limit".into());
    }
    stream
        .write_all(&encoded)
        .map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn protocol_methods_are_explicit_and_stable() {
        assert!(CollectorMethod::parse("GET_STATUS").is_ok());
        assert!(CollectorMethod::parse("WRITE_DATABASE").is_err());
    }

    #[test]
    fn request_reader_is_newline_delimited_and_bounded() {
        assert_eq!(
            read_limited_line(&mut Cursor::new(b"{}\ntrailing")).unwrap(),
            b"{}".to_vec()
        );
        assert!(read_limited_line(&mut Cursor::new(vec![b'x'; MAX_REQUEST_BYTES + 1])).is_err());
        assert!(read_limited_line(&mut Cursor::new(b"\n")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_is_owned_and_mode_0600() {
        use std::{
            fs,
            os::unix::fs::{MetadataExt, PermissionsExt},
            time::{SystemTime, UNIX_EPOCH},
        };

        let endpoint = std::env::temp_dir().join(format!(
            "codex-nexus-ipc-test-{}-{}.sock",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = bind_secure_listener(&endpoint).unwrap();
        let metadata = fs::symlink_metadata(&endpoint).unwrap();
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        validate_endpoint(&endpoint).unwrap();
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_endpoint(&endpoint).is_err());
        drop(listener);
        let _ = fs::remove_file(endpoint);
    }
}

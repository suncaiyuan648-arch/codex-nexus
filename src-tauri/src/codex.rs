use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        mpsc,
        Arc, Condvar, Mutex, Weak,
    },
    thread,
    time::Duration,
};

use tauri::{AppHandle, Emitter};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

type RpcResult = Result<Value, String>;
type PendingMap = HashMap<i64, mpsc::Sender<RpcResult>>;

#[derive(Debug)]
struct CodexCommand {
    program: String,
    prefix_args: Vec<String>,
    display_path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    pub phase: String,
    pub generation: u64,
    pub attempt: u32,
    pub retry_in_ms: Option<u64>,
    pub last_error: Option<String>,
    pub codex_path: Option<String>,
}

impl Default for ConnectionStatus {
    fn default() -> Self {
        Self {
            phase: "disconnected".into(),
            generation: 0,
            attempt: 0,
            retry_in_ms: None,
            last_error: None,
            codex_path: None,
        }
    }
}

pub struct CodexRpcClient {
    app: AppHandle,

    writer: Mutex<Option<mpsc::Sender<Value>>>,
    child: Mutex<Option<Child>>,
    pending: Mutex<PendingMap>,

    next_id: AtomicI64,
    next_generation: AtomicU64,
    active_generation: AtomicU64,

    reconnecting: AtomicBool,
    reconnect_wait: Condvar,
    reconnect_wait_lock: Mutex<bool>,
    shutting_down: AtomicBool,

    status: Mutex<ConnectionStatus>,
    display_path: Mutex<Option<String>>,
}

impl CodexRpcClient {
    pub fn start(app: AppHandle) -> Arc<Self> {
        let client = Arc::new(Self {
            app,
            writer: Mutex::new(None),
            child: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            next_generation: AtomicU64::new(1),
            active_generation: AtomicU64::new(0),
            reconnecting: AtomicBool::new(false),
            reconnect_wait: Condvar::new(),
            reconnect_wait_lock: Mutex::new(false),
            shutting_down: AtomicBool::new(false),
            status: Mutex::new(ConnectionStatus::default()),
            display_path: Mutex::new(None),
        });

        client.set_status(
            "connecting",
            0,
            None,
            None,
        );

        if let Err(error) = client.establish_connection(0) {
            client.set_status(
                "disconnected",
                0,
                Some(error.clone()),
                None,
            );

            eprintln!("[Codex RPC] initial connection failed: {error}");
            client.schedule_reconnect();
        }

        client
    }

    pub fn request(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
    ) -> RpcResult {
        let status = self.status();

        if status.phase != "ready" {
            return Err(format!(
                "Codex connection is not ready (state={})",
                status.phase
            ));
        }

        self.request_internal(
            method,
            params,
            REQUEST_TIMEOUT,
        )
    }

    pub fn status(&self) -> ConnectionStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| ConnectionStatus {
                phase: "disconnected".into(),
                last_error: Some("Connection status lock poisoned".into()),
                ..ConnectionStatus::default()
            })
    }

    pub fn reconnect(self: &Arc<Self>) -> Result<(), String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("Codex RPC client is shutting down".into());
        }

        let status = self.status();

        if status.phase != "disconnected"
            && status.phase != "reconnecting"
        {
            return Err(format!(
                "Codex connection is not disconnected (state={})",
                status.phase
            ));
        }

        self.set_status(
            "reconnecting",
            status.attempt.max(1),
            status.last_error,
            Some(0),
        );

        if let Ok(mut requested) = self.reconnect_wait_lock.lock() {
            *requested = true;
        }

        self.reconnect_wait.notify_all();
        self.schedule_reconnect();

        Ok(())
    }

    pub fn display_path(&self) -> Option<String> {
        self.display_path
            .lock()
            .ok()
            .and_then(|value| value.clone())
    }

    fn establish_connection(
        self: &Arc<Self>,
        attempt: u32,
    ) -> Result<(), String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("Codex RPC client is shutting down".into());
        }

        self.set_status(
            if attempt == 0 { "connecting" } else { "reconnecting" },
            attempt,
            None,
            None,
        );

        let command = resolve_codex()?;
        let mut child = spawn_app_server(&command)?;
        let pid = child.id();

        let stdin = child
            .stdin
            .take()
            .ok_or("Codex stdin unavailable")?;

        let stdout = child
            .stdout
            .take()
            .ok_or("Codex stdout unavailable")?;

        let stderr = child
            .stderr
            .take()
            .ok_or("Codex stderr unavailable")?;

        let generation = self
            .next_generation
            .fetch_add(1, Ordering::Relaxed);

        let (writer_tx, writer_rx) = mpsc::channel::<Value>();

        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| "Codex writer lock poisoned".to_string())?;

            *writer = Some(writer_tx);
        }

        {
            let mut child_slot = self
                .child
                .lock()
                .map_err(|_| "Codex child lock poisoned".to_string())?;

            if let Some(mut old_child) = child_slot.take() {
                let _ = old_child.kill();
                let _ = old_child.wait();
            }

            *child_slot = Some(child);
        }

        {
            let mut path = self
                .display_path
                .lock()
                .map_err(|_| "Codex path lock poisoned".to_string())?;

            *path = Some(command.display_path.clone());
        }

        self.active_generation
            .store(generation, Ordering::Release);

        println!(
            "[Codex RPC] app-server started generation={generation} pid={pid} path={}",
            command.display_path
        );

        self.spawn_writer(generation, stdin, writer_rx);
        self.spawn_stdout_reader(generation, stdout);
        self.spawn_stderr_reader(stderr);

        self.set_status(
            "initializing",
            attempt,
            None,
            None,
        );

        let initialize_result = self.request_internal(
            "initialize",
            Some(json!({
                "clientInfo": {
                    "name": "codex_nexus",
                    "title": "Codex Nexus",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            Duration::from_secs(12),
        );

        if let Err(error) = initialize_result {
            self.disconnect_generation(
                generation,
                format!("initialize failed: {error}"),
                false,
            );

            return Err(error);
        }

        if let Err(error) = self.notify_internal(
            "initialized",
            Some(json!({})),
        ) {
            self.disconnect_generation(
                generation,
                format!("initialized notification failed: {error}"),
                false,
            );

            return Err(error);
        }

        self.set_status(
            "ready",
            attempt,
            None,
            None,
        );

        println!(
            "[Codex RPC] ready generation={generation}"
        );

        Ok(())
    }

    fn spawn_writer(
        self: &Arc<Self>,
        generation: u64,
        stdin: impl Write + Send + 'static,
        writer_rx: mpsc::Receiver<Value>,
    ) {
        let weak: Weak<Self> = Arc::downgrade(self);

        thread::spawn(move || {
            let mut stdin = stdin;

            for message in writer_rx {
                if let Err(error) = write_rpc(&mut stdin, message) {
                    if let Some(client) = weak.upgrade() {
                        client.transport_failed(
                            generation,
                            format!("stdin write failed: {error}"),
                        );
                    }

                    break;
                }
            }
        });
    }

    fn spawn_stdout_reader(
        self: &Arc<Self>,
        generation: u64,
        stdout: impl std::io::Read + Send + 'static,
    ) {
        let weak: Weak<Self> = Arc::downgrade(self);

        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(line) => line,

                    Err(error) => {
                        if let Some(client) = weak.upgrade() {
                            client.transport_failed(
                                generation,
                                format!("stdout read failed: {error}"),
                            );
                        }

                        return;
                    }
                };

                if line.trim().is_empty() {
                    continue;
                }

                let message = match serde_json::from_str::<Value>(&line) {
                    Ok(message) => message,

                    Err(error) => {
                        eprintln!(
                            "[Codex RPC] invalid JSON generation={generation}: {error}"
                        );
                        continue;
                    }
                };

                let Some(client) = weak.upgrade() else {
                    return;
                };

                client.handle_message(
                    generation,
                    message,
                );
            }

            if let Some(client) = weak.upgrade() {
                client.transport_failed(
                    generation,
                    "stdout closed".into(),
                );
            }
        });
    }

    fn spawn_stderr_reader(
        &self,
        stderr: impl std::io::Read + Send + 'static,
    ) {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }

                        if line.contains(
                            "failed to refresh available models: timeout waiting for child process to exit",
                        ) {
                            continue;
                        }

                        eprintln!("[Codex RPC stderr] {line}");
                    }

                    Err(error) => {
                        eprintln!("[Codex RPC] stderr read failed: {error}");
                        return;
                    }
                }
            }
        });
    }

    fn handle_message(
        self: &Arc<Self>,
        generation: u64,
        message: Value,
    ) {
        if self.active_generation.load(Ordering::Acquire) != generation {
            return;
        }

        if let Some(id) = message
            .get("id")
            .and_then(Value::as_i64)
        {
            if message.get("method").is_some() {
                eprintln!(
                    "[Codex RPC] unhandled server request generation={generation}: {message}"
                );
                return;
            }

            #[cfg(debug_assertions)]
            println!("[Codex RPC] <- #{id}");

            let result = if let Some(error) = message.get("error") {
                Err(error.to_string())
            } else {
                Ok(
                    message
                        .get("result")
                        .cloned()
                        .unwrap_or(Value::Null)
                )
            };

            let sender = self
                .pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id));

            if let Some(sender) = sender {
                let _ = sender.send(result);
            }

            return;
        }

        let Some(method) = message
            .get("method")
            .and_then(Value::as_str)
        else {
            return;
        };

        #[cfg(debug_assertions)]
        println!("[Codex RPC] notification: {method}");

        if method == "account/rateLimits/updated" {
            if let Some(params) = message.get("params") {
                println!("[Codex RPC] rate limits updated");

                if let Err(error) = self.app.emit(
                    "codex://rate-limits-updated",
                    params.clone(),
                ) {
                    eprintln!(
                        "[Tauri] failed to emit rate-limit update: {error}"
                    );
                }
            }
        }

        if method == "account/updated" {
            println!("[Codex RPC] account updated");

            if let Err(error) = self.app.emit(
                "codex://account-updated",
                message
                    .get("params")
                    .cloned()
                    .unwrap_or(Value::Null),
            ) {
                eprintln!(
                    "[Tauri] failed to emit account update: {error}"
                );
            }
        }
    }

    fn request_internal(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> RpcResult {
        let id = self.next_id.fetch_add(
            1,
            Ordering::Relaxed,
        );

        let generation = self.active_generation.load(Ordering::Acquire);

        if generation == 0 {
            return Err("Codex transport is disconnected".into());
        }

        let (response_tx, response_rx) =
            mpsc::channel::<RpcResult>();

        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| "Codex pending map poisoned".to_string())?;

            pending.insert(
                id,
                response_tx,
            );
        }

        let mut message = json!({
            "method": method,
            "id": id
        });

        if let Some(params) = params {
            message["params"] = params;
        }

        #[cfg(debug_assertions)]
        println!("[Codex RPC] -> #{id} {method}");

        let writer = self
            .writer
            .lock()
            .map_err(|_| "Codex writer lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "Codex writer unavailable".to_string())?;

        if let Err(error) = writer.send(message) {
            self.remove_pending(id);

            self.transport_failed(
                generation,
                format!("writer channel disconnected: {error}"),
            );

            return Err("Codex writer disconnected".into());
        }

        match response_rx.recv_timeout(timeout) {
            Ok(result) => result,

            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.remove_pending(id);

                self.transport_failed(
                    generation,
                    format!("RPC `{method}` timed out"),
                );

                Err(format!(
                    "Timed out waiting for Codex RPC `{method}`"
                ))
            }

            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.remove_pending(id);

                self.transport_failed(
                    generation,
                    format!("RPC `{method}` response channel disconnected"),
                );

                Err(format!(
                    "Codex RPC `{method}` disconnected"
                ))
            }
        }
    }

    fn notify_internal(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let mut message = json!({
            "method": method
        });

        if let Some(params) = params {
            message["params"] = params;
        }

        let writer = self
            .writer
            .lock()
            .map_err(|_| "Codex writer lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "Codex writer unavailable".to_string())?;

        writer
            .send(message)
            .map_err(|error| {
                format!("Codex writer unavailable: {error}")
            })
    }

    fn transport_failed(
        self: &Arc<Self>,
        generation: u64,
        reason: String,
    ) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }

        self.disconnect_generation(
            generation,
            reason,
            true,
        );
    }

    fn disconnect_generation(
        self: &Arc<Self>,
        generation: u64,
        reason: String,
        schedule_reconnect: bool,
    ) {
        if self.active_generation
            .compare_exchange(
                generation,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }

        eprintln!(
            "[Codex RPC] disconnected generation={generation}: {reason}"
        );

        if let Ok(mut writer) = self.writer.lock() {
            *writer = None;
        }

        if let Ok(mut child_slot) = self.child.lock() {
            if let Some(mut child) = child_slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        self.fail_all_pending(
            format!("Codex disconnected: {reason}")
        );

        self.set_status(
            "disconnected",
            0,
            Some(reason),
            None,
        );

        if schedule_reconnect {
            self.schedule_reconnect();
        }
    }

    fn schedule_reconnect(
        self: &Arc<Self>,
    ) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }

        if self.reconnecting.swap(
            true,
            Ordering::AcqRel,
        ) {
            return;
        }

        let weak = Arc::downgrade(self);

        thread::spawn(move || {
            let mut attempt = 1u32;

            loop {
                let Some(client) = weak.upgrade() else {
                    return;
                };

                if client.shutting_down.load(Ordering::Acquire) {
                    client.reconnecting.store(false, Ordering::Release);
                    return;
                }

                let forced = client.take_reconnect_request();
                let delay = if forced {
                    Duration::ZERO
                } else {
                    reconnect_delay(attempt)
                };

                let last_error = client
                    .status
                    .lock()
                    .ok()
                    .and_then(|status| status.last_error.clone());

                client.set_status(
                    "reconnecting",
                    attempt,
                    last_error,
                    Some(delay.as_millis() as u64),
                );

                println!(
                    "[Codex RPC] reconnect attempt={attempt} in {}ms",
                    delay.as_millis()
                );

                if !forced && client.wait_for_reconnect(delay) {
                    println!(
                        "[Codex RPC] manual reconnect requested"
                    );
                }

                if client.shutting_down.load(Ordering::Acquire) {
                    client.reconnecting.store(false, Ordering::Release);
                    return;
                }

                match client.establish_connection(attempt) {
                    Ok(()) => {
                        client.clear_reconnect_request();
                        client.reconnecting.store(
                            false,
                            Ordering::Release,
                        );

                        println!(
                            "[Codex RPC] reconnect succeeded attempt={attempt}"
                        );

                        return;
                    }

                    Err(error) => {
                        eprintln!(
                            "[Codex RPC] reconnect attempt={attempt} failed: {error}"
                        );

                        client.set_status(
                            "disconnected",
                            attempt,
                            Some(error),
                            None,
                        );

                        attempt = attempt.saturating_add(1);
                    }
                }
            }
        });
    }

    fn set_status(
        &self,
        phase: &str,
        attempt: u32,
        last_error: Option<String>,
        retry_in_ms: Option<u64>,
    ) {
        let codex_path = self
            .display_path
            .lock()
            .ok()
            .and_then(|value| value.clone());

        let generation =
            self.active_generation.load(Ordering::Acquire);

        let status = ConnectionStatus {
            phase: phase.to_string(),
            generation,
            attempt,
            retry_in_ms,
            last_error,
            codex_path,
        };

        if let Ok(mut current) = self.status.lock() {
            *current = status.clone();
        }

        let _ = self.app.emit(
            "codex://connection-state",
            status,
        );
    }

    fn fail_all_pending(
        &self,
        error: String,
    ) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };

        for (_, sender) in pending.drain() {
            let _ = sender.send(
                Err(error.clone())
            );
        }
    }

    fn remove_pending(
        &self,
        id: i64,
    ) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    fn wait_for_reconnect(
        &self,
        delay: Duration,
    ) -> bool {
        let Ok(requested) = self.reconnect_wait_lock.lock() else {
            thread::sleep(delay);
            return false;
        };

        let Ok((mut requested, _)) = self
            .reconnect_wait
            .wait_timeout_while(
                requested,
                delay,
                |requested| !*requested,
            )
        else {
            return false;
        };

        let forced = *requested;
        *requested = false;
        forced
    }

    fn take_reconnect_request(&self) -> bool {
        let Ok(mut requested) = self.reconnect_wait_lock.lock() else {
            return false;
        };

        let forced = *requested;
        *requested = false;
        forced
    }

    fn clear_reconnect_request(&self) {
        if let Ok(mut requested) = self.reconnect_wait_lock.lock() {
            *requested = false;
        }
    }
}

impl Drop for CodexRpcClient {
    fn drop(&mut self) {
        self.shutting_down.store(
            true,
            Ordering::Release,
        );

        self.reconnect_wait.notify_all();

        self.active_generation.store(
            0,
            Ordering::Release,
        );

        if let Ok(mut writer) = self.writer.lock() {
            *writer = None;
        }

        if let Ok(mut child_slot) = self.child.lock() {
            if let Some(mut child) = child_slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        if let Ok(mut pending) = self.pending.lock() {
            for (_, sender) in pending.drain() {
                let _ = sender.send(
                    Err("Codex RPC client shutting down".into())
                );
            }
        }
    }
}

fn reconnect_delay(
    attempt: u32,
) -> Duration {
    let seconds = match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        5 => 15,
        _ => 30,
    };

    Duration::from_secs(seconds)
}

fn spawn_app_server(
    command: &CodexCommand,
) -> Result<Child, String> {
    let mut cmd = Command::new(&command.program);

    cmd.args(&command.prefix_args)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    cmd.spawn().map_err(|error| {
        format!("Failed to start Codex app-server: {error}")
    })
}

fn write_rpc(
    stdin: &mut impl Write,
    value: Value,
) -> Result<(), String> {
    let line = serde_json::to_string(&value)
        .map_err(|error| error.to_string())?;

    stdin
        .write_all(line.as_bytes())
        .map_err(|error| error.to_string())?;

    stdin
        .write_all(b"\n")
        .map_err(|error| error.to_string())?;

    stdin
        .flush()
        .map_err(|error| error.to_string())?;

    Ok(())
}

fn path_is_file(
    path: &Path,
) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn first_existing(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    paths
        .into_iter()
        .find(|path| path_is_file(path))
}

fn path_entries_from_env() -> Vec<PathBuf> {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn executable_candidates_in_path() -> Vec<PathBuf> {
    let names = if cfg!(target_os = "windows") {
        vec![
            "codex.exe",
            "codex.cmd",
            "codex.bat",
        ]
    } else {
        vec!["codex"]
    };

    path_entries_from_env()
        .into_iter()
        .flat_map(|directory| {
            names
                .iter()
                .map(move |name| directory.join(name))
        })
        .filter(|path| path_is_file(path))
        .collect()
}

fn shell_path_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return Vec::new();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let shell = env::var("SHELL")
            .unwrap_or_else(|_| "/bin/sh".into());

        let output = Command::new(shell)
            .args(["-ilc", "command -v codex"])
            .output();

        output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        let path = PathBuf::from(line.trim());

                        if path_is_file(&path) {
                            Some(path)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn macos_codex_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut candidates = vec![
            PathBuf::from(
                "/Applications/ChatGPT.app/Contents/Resources/codex",
            ),
            PathBuf::from(
                "/Applications/Codex.app/Contents/Resources/codex",
            ),
        ];

        if let Ok(home) = env::var("HOME") {
            let home = PathBuf::from(home);

            candidates.extend([
                home.join("Applications/ChatGPT.app/Contents/Resources/codex"),
                home.join("Applications/Codex.app/Contents/Resources/codex"),
            ]);
        }

        candidates
    }

    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

fn unix_codex_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return Vec::new();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut candidates = vec![
            PathBuf::from("/opt/homebrew/bin/codex"),
            PathBuf::from("/usr/local/bin/codex"),
            PathBuf::from("/usr/bin/codex"),
        ];

        if let Ok(home) = env::var("HOME") {
            let home = PathBuf::from(home);

            candidates.extend([
                home.join(".local/bin/codex"),
                home.join(".npm-global/bin/codex"),
                home.join(".volta/bin/codex"),
                home.join(".nvm/current/bin/codex"),
                home.join(".fnm/current/bin/codex"),
            ]);
        }

        if let Ok(path) = env::var("NVM_BIN") {
            candidates.push(PathBuf::from(path).join("codex"));
        }

        if let Ok(path) = env::var("FNM_MULTISHELL_PATH") {
            candidates.push(PathBuf::from(path).join("codex"));
        }

        if let Ok(path) = env::var("VOLTA_HOME") {
            candidates.push(PathBuf::from(path).join("bin/codex"));
        }

        candidates
    }
}

fn resolve_codex()
    -> Result<CodexCommand, String>
{
    if let Ok(explicit) = env::var("CODEX_BIN") {
        let path = PathBuf::from(&explicit);

        if path_is_file(&path) {
            return Ok(command_for_path(path));
        }

        return Err(format!(
            "CODEX_BIN points to a missing file: {explicit}"
        ));
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            let native =
                PathBuf::from(local_app_data)
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin")
                    .join("codex.exe");

            if path_is_file(&native) {
                return Ok(command_for_path(native));
            }
        }

        if let Ok(output) =
            Command::new("where.exe")
                .arg("codex")
                .output()
        {
            if output.status.success() {
                let paths: Vec<PathBuf> =
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(PathBuf::from)
                        .filter(|path| path_is_file(path))
                        .collect();

                if let Some(exe) =
                    paths.iter().find(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| {
                                ext.eq_ignore_ascii_case("exe")
                            })
                            .unwrap_or(false)
                    })
                {
                    return Ok(
                        command_for_path(exe.clone())
                    );
                }

                if let Some(script) =
                    paths.iter().find(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| {
                                ext.eq_ignore_ascii_case("cmd")
                                    || ext.eq_ignore_ascii_case("bat")
                            })
                            .unwrap_or(false)
                    })
                {
                    return Ok(
                        command_for_path(script.clone())
                    );
                }
            }
        }

        let mut candidates = Vec::new();

        if let Ok(appdata) = env::var("APPDATA") {
            candidates.push(
                PathBuf::from(appdata)
                    .join("npm")
                    .join("codex.cmd"),
            );
        }

        if let Some(path) = first_existing(candidates) {
            return Ok(command_for_path(path));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut candidates = Vec::new();
        candidates.extend(executable_candidates_in_path());
        candidates.extend(shell_path_candidates());
        candidates.extend(macos_codex_candidates());
        candidates.extend(unix_codex_candidates());

        if let Some(path) = first_existing(candidates) {
            return Ok(command_for_path(path));
        }
    }

    Err(
        "Cannot find `codex`. Checked PATH, the login shell PATH, common package-manager directories, and platform app bundles. Run `codex --version` or set CODEX_BIN."
            .into(),
    )
}

fn command_for_path(
    path: PathBuf,
) -> CodexCommand {
    let display_path =
        path.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        let ext =
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();

        if ext == "cmd" || ext == "bat" {
            return CodexCommand {
                program: "cmd.exe".into(),
                prefix_args: vec![
                    "/C".into(),
                    display_path.clone(),
                ],
                display_path,
            };
        }
    }

    CodexCommand {
        program: display_path.clone(),
        prefix_args: Vec::new(),
        display_path,
    }
}

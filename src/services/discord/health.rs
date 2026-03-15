use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::SharedData;


/// Per-provider snapshot for the health response.
struct ProviderEntry {
    name: String,
    shared: Arc<SharedData>,
}

/// Registry that providers register with so the health server can query all of them.
pub struct HealthRegistry {
    providers: tokio::sync::Mutex<Vec<ProviderEntry>>,
    started_at: Instant,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
        }
    }

    pub(super) async fn register(&self, name: String, shared: Arc<SharedData>) {
        self.providers.lock().await.push(ProviderEntry { name, shared });
    }
}

/// Start the health check HTTP server on the given port.
/// Runs forever — intended to be spawned as a background tokio task.
pub async fn serve(registry: Arc<HealthRegistry>, port: u16) {
    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 🩺 Health check server listening on {addr}");
            l
        }
        Err(e) => {
            eprintln!("  ⚠ Health check server failed to bind {addr}: {e}");
            return;
        }
    };

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let registry = registry.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);

            // Parse first line: "GET /api/health HTTP/1.1"
            let first_line = request.lines().next().unwrap_or("");
            let path = first_line.split_whitespace().nth(1).unwrap_or("");

            let (status, body) = if path == "/api/health" {
                let json = build_health_json(&registry).await;
                let healthy = is_healthy(&registry).await;
                let code = if healthy { "200 OK" } else { "503 Service Unavailable" };
                (code, json)
            } else {
                ("404 Not Found", r#"{"error":"not found"}"#.to_string())
            };

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

async fn build_health_json(registry: &HealthRegistry) -> String {
    let uptime_secs = registry.started_at.elapsed().as_secs();
    let version = env!("CARGO_PKG_VERSION");

    let providers = registry.providers.lock().await;
    let mut provider_entries = Vec::new();

    for entry in providers.iter() {
        let data = entry.shared.core.lock().await;
        let active_turns = data.cancel_tokens.len();
        let queue_depth: usize = data.intervention_queue.values().map(|q| q.len()).sum();
        let session_count = data.sessions.len();
        drop(data);

        let restart_pending = entry.shared.restart_pending.load(std::sync::atomic::Ordering::Relaxed);
        let connected = entry.shared.bot_connected.load(std::sync::atomic::Ordering::Relaxed);
        let last_turn_at = entry.shared.last_turn_at.lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|t| format!(r#""{}""#, t))
            .unwrap_or_else(|| "null".to_string());

        provider_entries.push(format!(
            r#"{{"name":"{}","connected":{},"active_turns":{},"queue_depth":{},"sessions":{},"restart_pending":{},"last_turn_at":{}}}"#,
            entry.name, connected, active_turns, queue_depth, session_count, restart_pending, last_turn_at
        ));
    }

    let global_active = if let Some(p) = providers.first() {
        p.shared.global_active.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let global_finalizing = if let Some(p) = providers.first() {
        p.shared.global_finalizing.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };

    format!(
        r#"{{"status":"{}","version":"{}","uptime_secs":{},"global_active":{},"global_finalizing":{},"providers":[{}]}}"#,
        if is_healthy_inner(&providers) { "healthy" } else { "unhealthy" },
        version,
        uptime_secs,
        global_active,
        global_finalizing,
        provider_entries.join(",")
    )
}

async fn is_healthy(registry: &HealthRegistry) -> bool {
    let providers = registry.providers.lock().await;
    is_healthy_inner(&providers)
}

fn is_healthy_inner(providers: &[ProviderEntry]) -> bool {
    // Unhealthy if no providers registered (startup not complete)
    if providers.is_empty() {
        return false;
    }
    for p in providers {
        // Unhealthy if any provider hasn't connected to Discord gateway yet
        if !p.shared.bot_connected.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        // Unhealthy if restart is pending (draining)
        if p.shared.restart_pending.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
    }
    true
}

/// Resolve the health check port from env or default.
pub fn resolve_port() -> u16 {
    std::env::var("REMOTECC_HEALTH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8793)
}

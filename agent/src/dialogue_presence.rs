//! Optional etcd-lease presence holder for `agent dialogue watch`.
//!
//! The mailbox/presence model (mu push-mailbox spec §1): a consumer registers
//! ITS OWN mailbox as an etcd key held by a lease it keeps alive; the lease IS
//! the liveness proof and expires on death. The long-lived `watch` process is
//! the natural holder for a Claude Code peer — it runs for the session's
//! lifetime (the Stop-hook listener), so lease lifetime ≈ session lifetime.
//!
//! **Opt-in.** Nothing happens unless endpoints are configured:
//!
//! ```toml
//! # ~/.config/agent/config.toml
//! [dialogue]
//! presence_etcd = "http://<etcd-host>:2379"   # comma-separated endpoints
//! # presence_prefix = "/mu/dialogue/v1/peers/"
//! # presence_ttl_s  = 60
//! ```
//!
//! (env overrides: `AGENT_DIALOGUE_PRESENCE_ETCD`, `..._PREFIX`, `..._TTL_S`.)
//!
//! **Fail-open.** etcd trouble never kills the watch: registration retries on
//! the keepalive cadence and presence simply degrades to activity-derived
//! until etcd answers again. Transport is etcd's v3 JSON gateway over the
//! existing sync ureq client — no gRPC, no async runtime.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

const DEFAULT_PREFIX: &str = "/mu/dialogue/v1/peers/";
const DEFAULT_TTL_S: u64 = 60;
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

struct Config {
    endpoints: Vec<String>,
    prefix: String,
    ttl_s: u64,
}

fn config() -> Option<Config> {
    let eps =
        crate::embed::resolve_setting("dialogue", "presence_etcd", "AGENT_DIALOGUE_PRESENCE_ETCD")?;
    let endpoints: Vec<String> = eps
        .split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if endpoints.is_empty() {
        return None;
    }
    let prefix = crate::embed::resolve_setting(
        "dialogue",
        "presence_prefix",
        "AGENT_DIALOGUE_PRESENCE_PREFIX",
    )
    .unwrap_or_else(|| DEFAULT_PREFIX.to_string());
    let ttl_s = crate::embed::resolve_setting(
        "dialogue",
        "presence_ttl_s",
        "AGENT_DIALOGUE_PRESENCE_TTL_S",
    )
    .and_then(|s| s.parse().ok())
    .filter(|t| *t >= 5)
    .unwrap_or(DEFAULT_TTL_S);
    Some(Config {
        endpoints,
        prefix,
        ttl_s,
    })
}

/// POST one JSON-gateway call against the first endpoint that answers.
fn etcd_post(cfg: &Config, path: &str, body: &Value) -> Option<Value> {
    for ep in &cfg.endpoints {
        let url = format!("{ep}{path}");
        let resp = ureq::post(&url)
            .timeout(CALL_TIMEOUT)
            .send_json(body.clone());
        if let Ok(r) = resp {
            if let Ok(v) = r.into_json::<Value>() {
                return Some(v);
            }
        }
    }
    None
}

/// etcd's gateway emits int64s as JSON strings; tolerate both.
fn id_of(v: &Value, field: &str) -> Option<String> {
    match v.get(field)? {
        Value::String(s) if !s.is_empty() && s != "0" => Some(s.clone()),
        Value::Number(n) if n.as_i64().unwrap_or(0) != 0 => Some(n.to_string()),
        _ => None,
    }
}

/// Grant a lease and put the peer's presence key under it. Returns the lease
/// id on success.
fn register(cfg: &Config, peer_id: &str) -> Option<String> {
    let lease = id_of(
        &etcd_post(cfg, "/v3/lease/grant", &json!({"TTL": cfg.ttl_s}))?,
        "ID",
    )?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let role = peer_id.split(':').next().unwrap_or(peer_id);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let value = json!({
        "peer_id": peer_id, "role": role, "registered_at_unix_ms": now_ms,
    });
    etcd_post(
        cfg,
        "/v3/kv/put",
        &json!({
            "key": b64.encode(format!("{}{}", cfg.prefix, peer_id)),
            "value": b64.encode(value.to_string()),
            "lease": lease,
        }),
    )?;
    Some(lease)
}

/// Refresh the lease; false means it is gone (expired/revoked) and the caller
/// must re-register.
fn keepalive(cfg: &Config, lease: &str) -> bool {
    let Some(resp) = etcd_post(cfg, "/v3/lease/keepalive", &json!({"ID": lease})) else {
        return false;
    };
    // Gateway shape: {"result":{"ID":"...","TTL":"60"}}; TTL absent/0 = dead.
    resp.get("result")
        .and_then(|r| r.get("TTL"))
        .map(|t| match t {
            Value::String(s) => s.parse::<i64>().unwrap_or(0) > 0,
            Value::Number(n) => n.as_i64().unwrap_or(0) > 0,
            _ => false,
        })
        .unwrap_or(false)
}

/// If presence is configured, hold a lease-backed registration for `peer_id`
/// for the life of this process: register, then keepalive at TTL/3, silently
/// re-registering whenever the lease or etcd went away. No config → no-op.
/// The thread is detached on purpose — when the watch process dies, the lease
/// stops being refreshed and expires: that IS the deregistration.
pub fn hold(peer_id: &str) {
    let Some(cfg) = config() else {
        return;
    };
    let peer = peer_id.to_string();
    std::thread::spawn(move || {
        let mut lease = register(&cfg, &peer);
        if lease.is_some() {
            log::info!("dialogue presence: lease-registered {peer}");
        } else {
            log::warn!("dialogue presence: etcd unavailable; will keep retrying (fail-open)");
        }
        loop {
            std::thread::sleep(Duration::from_secs((cfg.ttl_s / 3).max(2)));
            let alive = lease
                .as_deref()
                .map(|l| keepalive(&cfg, l))
                .unwrap_or(false);
            if !alive {
                lease = register(&cfg, &peer);
                if lease.is_some() {
                    log::info!("dialogue presence: re-registered {peer}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_of_tolerates_string_and_number() {
        assert_eq!(
            id_of(&json!({"ID": "7587"}), "ID"),
            Some("7587".to_string())
        );
        assert_eq!(id_of(&json!({"ID": 42}), "ID"), Some("42".to_string()));
        assert_eq!(id_of(&json!({"ID": "0"}), "ID"), None);
        assert_eq!(id_of(&json!({}), "ID"), None);
    }
}

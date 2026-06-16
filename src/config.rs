//! Environment parsing + startup validation. Fail fast: a missing secret, a bad URL, or an
//! unknown policy enum aborts boot rather than silently degrading. Nothing here logs a secret;
//! validation errors name the offending *variable*, never its value.

use reqwest::Url;
use std::{collections::HashSet, env, net::SocketAddr, path::PathBuf, time::Duration};

use crate::{
    channel::{ChannelConfig, DEFAULT_CHANNEL},
    error::DynError,
};

pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
pub const DEFAULT_GOWA_URL: &str = "http://127.0.0.1:3000";
pub const DEFAULT_AGENT_URL: &str = "http://127.0.0.1:3001";
pub const DEFAULT_SEND_RATE_PER_MIN: u32 = 20;

/// Durable forward-queue defaults (see `crate::forward`).
pub const DEFAULT_QUEUE_DIR: &str = "/var/lib/wagw/shim/queue";
pub const DEFAULT_FORWARD_MAX_RETRIES: u32 = 5;
pub const DEFAULT_FORWARD_CONCURRENCY: usize = 4;
pub const DEFAULT_FORWARD_BACKOFF_MS: u64 = 1000;

/// Suffix of a WhatsApp DM (one-to-one) JID.
pub const DM_SUFFIX: &str = "@s.whatsapp.net";
/// Suffix of a WhatsApp group JID.
pub const GROUP_SUFFIX: &str = "@g.us";

/// Fully-resolved shim configuration. Built once at boot by [`Config::from_env`]; cloned cheaply
/// into the shared `AppState`. Secrets are owned `String`s — keep them off any `Debug`/log path.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub gowa_url: String,
    pub gowa_basic_auth: Option<BasicAuth>,
    pub gowa_device_id: String,
    pub gowa_webhook_secret: String,
    /// Full agent inbound endpoint, e.g. `http://127.0.0.1:3001/whatsapp/inbound`.
    pub agent_inbound_url: String,
    /// Agent readiness endpoint, e.g. `http://127.0.0.1:3001/health` (the agent exposes `/health`,
    /// not `/healthz`). Only probed by `/readyz` when `readyz_probe_agent` is set.
    pub agent_health_url: String,
    /// Bearer the shim *sends* to the agent on inbound forward.
    pub whatsapp_webhook_token: String,
    /// Bearer the shim *requires* on its own `POST /send` (the agent presents it).
    pub whatsapp_gateway_token: String,
    pub policy: PolicyConfig,
    pub send_rate_per_min: u32,
    /// When set, `/readyz` also probes the (now-peered) agent's `/health`. Default off: the agent is
    /// a separate box, so probing it from readiness adds cross-box coupling.
    pub readyz_probe_agent: bool,
    /// Debug sink mode (`SHIM_DEBUG_SINK`): run the gateway with **no agent target**. Inbound still
    /// flows through HMAC → parse → policy → durable queue, but the forward is logged and discarded
    /// instead of POSTed to the agent (so nothing dead-letters). Used to validate the GOWA⟷shim
    /// connection before the agent is wired. Never enable in front of real tenant traffic.
    pub agent_debug_sink: bool,
    /// Durable forward-queue root; holds `pending/` + `dead/` (see `crate::forward`).
    pub queue_dir: PathBuf,
    /// Max backoff retries before an inbound forward is dead-lettered.
    pub forward_max_retries: u32,
    /// Bound on concurrent agent forwards drained from the queue.
    pub forward_concurrency: usize,
    /// Base backoff between forward retries (doubles each attempt, capped internally).
    pub forward_backoff: Duration,
    /// Configured downstream channels (target endpoints). Always includes the synthesised
    /// `"default"` channel = today's single target, so an empty `WA_CHANNELS` behaves identically to
    /// before. See [`crate::channel`].
    pub channels: Vec<ChannelConfig>,
    /// Group JID → channel label routing map (from `WA_GROUP_CHANNELS`). Validated at load: every
    /// label is configured and every JID is a group JID.
    pub group_channels: Vec<(String, String)>,
}

/// GOWA HTTP basic-auth pair, parsed from `user:pass`.
#[derive(Clone)]
pub struct BasicAuth {
    pub user: String,
    pub pass: String,
}

/// DM admission mode (`WA_DM_POLICY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmPolicy {
    /// Answer any DM sender.
    Open,
    /// Answer only senders in `WA_DM_ALLOW`.
    Allowlist,
    /// Ignore all DMs.
    Off,
}

/// Group admission mode (`WA_GROUP_POLICY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupPolicy {
    /// Ignore all groups.
    Off,
    /// Consider only groups whose JID is in `WA_GROUP_ALLOW`.
    Allowlist,
    /// Consider every group (still subject to mention gating).
    Open,
}

/// Pure policy inputs, consumed by [`crate::policy`]. Cloned into the policy evaluator; holds no
/// runtime state of its own so it stays trivially unit-testable.
#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub dm_policy: DmPolicy,
    /// Allowed DM senders, normalised to `<number>@s.whatsapp.net`.
    pub dm_allow: Vec<String>,
    pub group_policy: GroupPolicy,
    /// Allowed group JIDs (`...@g.us`), used when `group_policy == Allowlist`.
    pub group_allow: Vec<String>,
    /// In allowed groups, require the bot be addressed (here: replied-to) before answering.
    pub require_mention: bool,
    /// Group JIDs that bypass `require_mention` entirely (free-for-all chats).
    pub free_response_chats: Vec<String>,
}

impl Config {
    /// Parse and validate the whole configuration from the process environment. Returns the first
    /// problem encountered as a boxed error; the caller aborts boot on `Err`.
    pub fn from_env() -> Result<Self, DynError> {
        let bind: SocketAddr = env_or("SHIM_BIND", DEFAULT_BIND)
            .parse()
            .map_err(|error| boxed(format!("SHIM_BIND is not a valid socket address: {error}")))?;

        let gowa_url = env_or("GOWA_URL", DEFAULT_GOWA_URL)
            .trim_end_matches('/')
            .to_string();
        validate_url("GOWA_URL", &gowa_url)?;

        let gowa_basic_auth = secret("GOWA_BASIC_AUTH")
            .map(|raw| BasicAuth::parse(&raw))
            .transpose()?;

        let gowa_device_id = required("GOWA_DEVICE_ID")?;
        let gowa_webhook_secret = required_secret("GOWA_WEBHOOK_SECRET")?;
        if gowa_webhook_secret == "secret" {
            return Err(boxed(
                "GOWA_WEBHOOK_SECRET is still GOWA's default 'secret' — set a per-tenant secret",
            ));
        }

        let agent_base = env_or("AGENT_INBOUND_URL", DEFAULT_AGENT_URL)
            .trim_end_matches('/')
            .to_string();
        let agent_inbound_url = format!("{agent_base}/whatsapp/inbound");
        validate_url("AGENT_INBOUND_URL", &agent_inbound_url)?;
        let agent_health_url = format!("{agent_base}/health");

        let whatsapp_webhook_token = required_secret("WHATSAPP_WEBHOOK_TOKEN")?;
        let whatsapp_gateway_token = required_secret("WHATSAPP_GATEWAY_TOKEN")?;

        let send_rate_per_min = match optional("WA_SEND_RATE_PER_MIN") {
            Some(raw) => raw
                .parse::<u32>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    boxed("WA_SEND_RATE_PER_MIN must be a positive integer".to_string())
                })?,
            None => DEFAULT_SEND_RATE_PER_MIN,
        };

        let mut policy = PolicyConfig::from_env()?;

        // --- Channels (all optional → backward compatible) ---
        // The default channel is always present and equals today's single target, so a config with no
        // `WA_CHANNELS` builds exactly one channel and behaves identically to before.
        let mut channels = vec![ChannelConfig {
            label: DEFAULT_CHANNEL.to_string(),
            inbound_url: agent_inbound_url.clone(),
            health_url: agent_health_url.clone(),
            bearer: whatsapp_webhook_token.clone(),
        }];
        let mut label_set: HashSet<String> = HashSet::new();
        label_set.insert(DEFAULT_CHANNEL.to_string());
        for label in list("WA_CHANNELS") {
            validate_channel_label(&label)?;
            if !label_set.insert(label.clone()) {
                return Err(boxed(format!(
                    "WA_CHANNELS lists label {label:?} more than once"
                )));
            }
            let upper = label.to_ascii_uppercase();
            let url_var = format!("WA_CHANNEL_{upper}_URL");
            let base = required(&url_var)?.trim_end_matches('/').to_string();
            let inbound_url = format!("{base}/whatsapp/inbound");
            validate_url(&url_var, &inbound_url)?;
            let health_url = format!("{base}/health");
            // Per-channel bearer is optional (via `secret()`, so `_FILE` works); it defaults to the
            // shim's single webhook token so two groups pointing at the same agent share its bearer.
            let bearer = secret(&format!("WA_CHANNEL_{upper}_TOKEN"))
                .unwrap_or_else(|| whatsapp_webhook_token.clone());
            channels.push(ChannelConfig {
                label,
                inbound_url,
                health_url,
                bearer,
            });
        }

        let group_channels = parse_group_channels(&env_or("WA_GROUP_CHANNELS", ""))?;
        validate_group_channel_labels(&group_channels, &label_set)?;

        // Admission convenience: a group you've assigned a channel is implicitly allowlisted, so it
        // need not also be listed in `WA_GROUP_ALLOW`. Policy itself stays pure — `require_mention`
        // and `group_policy=off` still apply.
        for (jid, _label) in &group_channels {
            if !policy.group_allow.iter().any(|allowed| allowed == jid) {
                policy.group_allow.push(jid.clone());
            }
        }

        let readyz_probe_agent = env_bool("SHIM_READYZ_PROBE_AGENT");
        let agent_debug_sink = env_bool("SHIM_DEBUG_SINK");
        let queue_dir = PathBuf::from(env_or("SHIM_QUEUE_DIR", DEFAULT_QUEUE_DIR));
        let forward_max_retries =
            parse_u32("SHIM_FORWARD_MAX_RETRIES", DEFAULT_FORWARD_MAX_RETRIES)?;
        let forward_concurrency = usize::try_from(parse_u32(
            "SHIM_FORWARD_CONCURRENCY",
            DEFAULT_FORWARD_CONCURRENCY as u32,
        )?)
        .unwrap_or(DEFAULT_FORWARD_CONCURRENCY)
        .max(1);
        let forward_backoff = Duration::from_millis(parse_u64(
            "SHIM_FORWARD_BACKOFF_MS",
            DEFAULT_FORWARD_BACKOFF_MS,
        )?);

        Ok(Self {
            bind,
            gowa_url,
            gowa_basic_auth,
            gowa_device_id,
            gowa_webhook_secret,
            agent_inbound_url,
            agent_health_url,
            whatsapp_webhook_token,
            whatsapp_gateway_token,
            policy,
            send_rate_per_min,
            readyz_probe_agent,
            agent_debug_sink,
            queue_dir,
            forward_max_retries,
            forward_concurrency,
            forward_backoff,
            channels,
            group_channels,
        })
    }
}

/// Validate a `WA_CHANNELS` label: `[a-z0-9_]`, non-empty, and not the reserved `"default"`.
fn validate_channel_label(label: &str) -> Result<(), DynError> {
    if label == DEFAULT_CHANNEL {
        return Err(boxed(format!(
            "WA_CHANNELS label {label:?} is reserved (the default channel is implicit)"
        )));
    }
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(boxed(format!(
            "WA_CHANNELS label {label:?} must match [a-z0-9_]"
        )));
    }
    Ok(())
}

/// Parse `WA_GROUP_CHANNELS` (`<jid>:<label>,<jid>:<label>`) into `(jid, label)` pairs. Pure (no
/// env), so it is unit-testable. Validates structure, that each JID is a group JID (`@g.us`), and
/// that no JID is mapped twice. Label-membership (does the label name a configured channel?) is a
/// separate check — see [`validate_group_channel_labels`] — because it needs the channel set.
pub fn parse_group_channels(raw: &str) -> Result<Vec<(String, String)>, DynError> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Split on the LAST colon: labels are `[a-z0-9_]` (colon-free) and group JIDs carry no
        // colon, so this cleanly separates `<jid>:<label>` either way.
        let (jid, label) = entry.rsplit_once(':').ok_or_else(|| {
            boxed(format!(
                "WA_GROUP_CHANNELS entry {entry:?} must be '<jid>:<label>'"
            ))
        })?;
        let jid = jid.trim();
        let label = label.trim();
        if jid.is_empty() || label.is_empty() {
            return Err(boxed(format!(
                "WA_GROUP_CHANNELS entry {entry:?} has an empty jid or label"
            )));
        }
        if !jid.ends_with(GROUP_SUFFIX) {
            return Err(boxed(format!(
                "WA_GROUP_CHANNELS jid {jid:?} is not a group JID (must end with {GROUP_SUFFIX})"
            )));
        }
        if !seen.insert(jid.to_string()) {
            return Err(boxed(format!(
                "WA_GROUP_CHANNELS maps jid {jid:?} more than once"
            )));
        }
        out.push((jid.to_string(), label.to_string()));
    }
    Ok(out)
}

/// Ensure every label in a parsed group→channel map names a configured channel. Pure (the label set
/// is passed in) so it is unit-testable without the environment.
fn validate_group_channel_labels(
    mappings: &[(String, String)],
    known_labels: &HashSet<String>,
) -> Result<(), DynError> {
    for (jid, label) in mappings {
        if !known_labels.contains(label) {
            return Err(boxed(format!(
                "WA_GROUP_CHANNELS maps {jid:?} to unknown channel {label:?} (not in WA_CHANNELS)"
            )));
        }
    }
    Ok(())
}

impl BasicAuth {
    /// Split `user:pass` on the first colon (passwords may contain colons).
    fn parse(raw: &str) -> Result<Self, DynError> {
        let (user, pass) = raw
            .split_once(':')
            .ok_or_else(|| boxed("GOWA_BASIC_AUTH must be in 'user:pass' form".to_string()))?;
        if user.is_empty() {
            return Err(boxed("GOWA_BASIC_AUTH user part is empty".to_string()));
        }
        Ok(Self {
            user: user.to_string(),
            pass: pass.to_string(),
        })
    }
}

impl PolicyConfig {
    fn from_env() -> Result<Self, DynError> {
        let dm_policy = match env_or("WA_DM_POLICY", "allowlist")
            .to_ascii_lowercase()
            .as_str()
        {
            "open" => DmPolicy::Open,
            "allowlist" => DmPolicy::Allowlist,
            "off" => DmPolicy::Off,
            other => {
                return Err(boxed(format!(
                    "WA_DM_POLICY must be open|allowlist|off, got {other:?}"
                )));
            }
        };
        let group_policy = match env_or("WA_GROUP_POLICY", "off")
            .to_ascii_lowercase()
            .as_str()
        {
            "off" => GroupPolicy::Off,
            "allowlist" => GroupPolicy::Allowlist,
            "open" => GroupPolicy::Open,
            other => {
                return Err(boxed(format!(
                    "WA_GROUP_POLICY must be off|allowlist|open, got {other:?}"
                )));
            }
        };

        Ok(Self {
            dm_policy,
            dm_allow: list("WA_DM_ALLOW")
                .into_iter()
                .map(|item| normalise_dm_jid(&item))
                .collect(),
            group_policy,
            group_allow: list("WA_GROUP_ALLOW"),
            require_mention: env_bool("WA_REQUIRE_MENTION"),
            free_response_chats: list("WA_FREE_RESPONSE_CHATS"),
        })
    }
}

/// Normalise a configured DM allow-entry to a full JID. Accepts a bare number (`61400111222`),
/// a `+`-prefixed number, or an already-suffixed JID; strips spaces and a leading `+`.
pub fn normalise_dm_jid(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.ends_with(DM_SUFFIX) {
        return trimmed.to_string();
    }
    let digits = trimmed.trim_start_matches('+');
    format!("{digits}{DM_SUFFIX}")
}

// --- small env helpers (mirror the agent's `non_empty` discipline) ---

fn boxed(message: impl Into<String>) -> DynError {
    Box::<dyn std::error::Error + Send + Sync>::from(message.into())
}

fn optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required(name: &str) -> Result<String, DynError> {
    optional(name).ok_or_else(|| boxed(format!("{name} is required but not set")))
}

/// Resolve a secret, preferring the direct env var, falling back to the file named by `<name>_FILE`.
/// This is the systemd-credential route (`LoadCredential=` + `<name>_FILE=%d/<cred>`): the value
/// lives in a 0600 file rather than the process environment (`/proc/<pid>/environ`). Direct env wins
/// so manual/foreground runs and ad-hoc overrides keep working.
fn secret(name: &str) -> Option<String> {
    secret_from(optional(name), optional(&format!("{name}_FILE")))
}

/// Pure core of [`secret`]: direct value wins; otherwise read + trim the file at `file_path`. Kept
/// separate from env access so it is unit-testable without mutating the process environment.
fn secret_from(direct: Option<String>, file_path: Option<String>) -> Option<String> {
    if let Some(value) = direct {
        return Some(value);
    }
    let path = file_path?;
    std::fs::read_to_string(path)
        .ok()
        .map(|contents| contents.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Like [`secret`] but required: errors naming the variable (and its `_FILE` form) — never the value.
fn required_secret(name: &str) -> Result<String, DynError> {
    secret(name).ok_or_else(|| boxed(format!("{name} is required (set {name} or {name}_FILE)")))
}

fn env_or(name: &str, default: &str) -> String {
    optional(name).unwrap_or_else(|| default.to_string())
}

fn env_bool(name: &str) -> bool {
    matches!(optional(name).as_deref(), Some("1" | "true" | "yes" | "on"))
}

/// Parse a comma-separated env var into a trimmed, non-empty list (order preserved).
fn list(name: &str) -> Vec<String> {
    optional(name)
        .map(|raw| {
            raw.split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an optional non-negative integer env var, falling back to `default` when unset. An
/// explicitly-set but unparseable value is an error (fail fast) rather than a silent default.
fn parse_u32(name: &str, default: u32) -> Result<u32, DynError> {
    match optional(name) {
        Some(raw) => raw
            .parse::<u32>()
            .map_err(|_| boxed(format!("{name} must be a non-negative integer"))),
        None => Ok(default),
    }
}

fn parse_u64(name: &str, default: u64) -> Result<u64, DynError> {
    match optional(name) {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| boxed(format!("{name} must be a non-negative integer"))),
        None => Ok(default),
    }
}

fn validate_url(name: &str, value: &str) -> Result<(), DynError> {
    Url::parse(value).map_err(|error| boxed(format!("{name} is not a valid URL: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_bare_number_to_dm_jid() {
        assert_eq!(
            normalise_dm_jid("61400111222"),
            "61400111222@s.whatsapp.net"
        );
        assert_eq!(
            normalise_dm_jid(" +61400111222 "),
            "61400111222@s.whatsapp.net"
        );
        assert_eq!(
            normalise_dm_jid("61400111222@s.whatsapp.net"),
            "61400111222@s.whatsapp.net"
        );
    }

    #[test]
    fn basic_auth_splits_on_first_colon() {
        let auth = BasicAuth::parse("admin:p:a:ss").unwrap();
        assert_eq!(auth.user, "admin");
        assert_eq!(auth.pass, "p:a:ss");
    }

    #[test]
    fn basic_auth_rejects_missing_colon() {
        assert!(BasicAuth::parse("adminpass").is_err());
    }

    #[test]
    fn secret_prefers_direct_value_over_file() {
        let file = std::env::temp_dir().join(format!("wagw-secret-{}.txt", std::process::id()));
        std::fs::write(&file, "from-file").unwrap();
        // Direct env value wins even when a file is also present.
        let resolved = secret_from(
            Some("from-env".to_string()),
            Some(file.to_string_lossy().into_owned()),
        );
        assert_eq!(resolved.as_deref(), Some("from-env"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn secret_reads_and_trims_file_when_direct_absent() {
        let file =
            std::env::temp_dir().join(format!("wagw-secret-file-{}.txt", std::process::id()));
        // A trailing newline (as a credential file or `echo >` would leave) must be trimmed off.
        std::fs::write(&file, "  s3cr3t-token\n").unwrap();
        let resolved = secret_from(None, Some(file.to_string_lossy().into_owned()));
        assert_eq!(resolved.as_deref(), Some("s3cr3t-token"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn secret_is_none_when_neither_present_or_file_empty() {
        assert_eq!(secret_from(None, None), None);
        let file =
            std::env::temp_dir().join(format!("wagw-secret-empty-{}.txt", std::process::id()));
        std::fs::write(&file, "   \n").unwrap(); // whitespace-only → treated as absent
        assert_eq!(
            secret_from(None, Some(file.to_string_lossy().into_owned())),
            None
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn parse_group_channels_happy_path() {
        let parsed =
            parse_group_channels(" 120363000000000000@g.us:support , 120363111111111111@g.us:ops ")
                .unwrap();
        assert_eq!(
            parsed,
            vec![
                ("120363000000000000@g.us".to_string(), "support".to_string()),
                ("120363111111111111@g.us".to_string(), "ops".to_string()),
            ]
        );
        // Empty / whitespace input → no mappings (the default-only case).
        assert!(parse_group_channels("").unwrap().is_empty());
        assert!(parse_group_channels("   ").unwrap().is_empty());
    }

    #[test]
    fn parse_group_channels_rejects_non_group_jid() {
        // A DM JID is not routable per-group.
        let error = parse_group_channels("61400111222@s.whatsapp.net:support")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a group JID"));
        // A missing colon is malformed.
        assert!(parse_group_channels("120363000000000000@g.us").is_err());
    }

    #[test]
    fn parse_group_channels_rejects_duplicate_jid() {
        let error =
            parse_group_channels("120363000000000000@g.us:support,120363000000000000@g.us:ops")
                .unwrap_err()
                .to_string();
        assert!(error.contains("more than once"));
    }

    #[test]
    fn validate_group_channel_labels_rejects_unknown_label() {
        let known: HashSet<String> = [DEFAULT_CHANNEL.to_string(), "support".to_string()]
            .into_iter()
            .collect();
        let ok = vec![("120363000000000000@g.us".to_string(), "support".to_string())];
        assert!(validate_group_channel_labels(&ok, &known).is_ok());

        let unknown = vec![("120363000000000000@g.us".to_string(), "nope".to_string())];
        let error = validate_group_channel_labels(&unknown, &known)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown channel"));
    }

    #[test]
    fn validate_channel_label_enforces_charset_and_reserved() {
        assert!(validate_channel_label("support").is_ok());
        assert!(validate_channel_label("ops_2").is_ok());
        assert!(validate_channel_label("default").is_err()); // reserved
        assert!(validate_channel_label("Support").is_err()); // uppercase
        assert!(validate_channel_label("a-b").is_err()); // hyphen not allowed
        assert!(validate_channel_label("").is_err());
    }

    #[test]
    fn required_secret_error_names_the_variable_not_the_value() {
        // A definitely-unset var: only reads env, never mutates it. Message must name the var.
        let error = required_secret("WAGW_TEST_DEFINITELY_UNSET_SECRET")
            .unwrap_err()
            .to_string();
        assert!(error.contains("WAGW_TEST_DEFINITELY_UNSET_SECRET"));
        assert!(error.contains("WAGW_TEST_DEFINITELY_UNSET_SECRET_FILE"));
    }
}

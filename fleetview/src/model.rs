//! Pure data layer for `fleetview`: read `deploy/fleet.yaml` + `deploy/tenants/*.yaml`, merge
//! them into one `Tenant` per id, and derive the display-facing bits (channel rows, health
//! signals, group display-names scraped from inline yaml comments).
//!
//! No terminal / no IO beyond reading the yaml files, so it is trivially unit-tested. The TUI in
//! `main.rs` only renders what this module produces. Secret VALUES are never touched — the yaml
//! carries secret NAMES and we pass those through verbatim.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Severity for a status pill or a health callout — drives colour only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sev {
    Good,
    Warn,
    Crit,
    Info,
    Muted,
}

// ---------------------------------------------------------------- wire structs

#[derive(Deserialize, Default)]
struct Policy {
    dm_policy: Option<String>,
    dm_allow: Option<Vec<String>>,
    group_policy: Option<String>,
    group_allow: Option<Vec<String>>,
    require_mention: Option<bool>,
    #[allow(dead_code)]
    free_response_chats: Option<Vec<String>>,
    send_rate_per_min: Option<u32>,
}

#[derive(Deserialize)]
struct ChannelDef {
    label: String,
    url: Option<String>,
    token_secret: Option<String>,
}

#[derive(Deserialize)]
struct GroupChannelDef {
    jid: String,
    channel: String,
}

/// A `deploy/tenants/<id>.yaml` document.
#[derive(Deserialize, Default)]
struct TenantFile {
    #[serde(rename = "box")]
    box_host: Option<String>,
    magicdns: Option<String>,
    device_jid: Option<String>,
    gowa_device_id: Option<String>,
    self_number: Option<String>,
    policy: Option<Policy>,
    channels: Option<Vec<ChannelDef>>,
    group_channels: Option<Vec<GroupChannelDef>>,
    agent_inbound_url: Option<String>,
    debug_sink: Option<bool>,
    secrets: Option<serde_yaml::Mapping>,
}

/// A `deploy/fleet.yaml` entry.
#[derive(Deserialize)]
struct FleetEntry {
    id: String,
    #[serde(rename = "box")]
    box_host: Option<String>,
    magicdns: Option<String>,
    wa_account: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize, Default)]
struct FleetIndex {
    tenants: Option<Vec<FleetEntry>>,
}

// ------------------------------------------------------------- resolved model

/// One channel edge out of the shim, including the implicit `default`.
pub struct ChannelRow {
    pub label: String,
    pub url: String,
    pub token: String,
    pub serves: String,
    pub implicit: bool,
    pub sink: bool,
}

/// A tenant after merging the fleet index and its config file.
pub struct Tenant {
    pub id: String,
    pub box_host: String,
    pub magicdns: String,
    pub wa_account: String,
    pub device_jid: String,
    pub gowa_device_id: String,
    pub self_number: String,
    pub status: String,
    pub status_sev: Sev,
    pub in_fleet: bool,
    pub has_config: bool,

    dm_policy: String,
    dm_allow: Vec<String>,
    group_policy: String,
    group_allow: Vec<String>,
    require_mention: bool,
    send_rate_per_min: u32,

    channels: Vec<ChannelDef>,
    group_channels: Vec<GroupChannelDef>,
    pub agent_inbound_url: String,
    pub debug_sink: bool,
    pub secrets: Vec<(String, String)>,
    pub group_labels: BTreeMap<String, String>,
    pub source: String,
}

pub struct Fleet {
    pub tenants: Vec<Tenant>,
    pub deploy_dir: PathBuf,
}

impl Tenant {
    pub fn dm_policy(&self) -> &str {
        &self.dm_policy
    }
    pub fn dm_allow(&self) -> &[String] {
        &self.dm_allow
    }
    pub fn group_policy(&self) -> &str {
        &self.group_policy
    }
    pub fn require_mention(&self) -> bool {
        self.require_mention
    }
    pub fn send_rate_per_min(&self) -> u32 {
        self.send_rate_per_min
    }

    /// Human name for a group jid, if one was scraped from an inline yaml comment.
    pub fn group_label(&self, jid: &str) -> Option<&str> {
        self.group_labels.get(jid).map(String::as_str)
    }

    fn defined_labels(&self) -> Vec<&str> {
        self.channels.iter().map(|c| c.label.as_str()).collect()
    }

    /// Which groups route to each channel label.
    fn routed(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut m: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for gc in &self.group_channels {
            m.entry(gc.channel.as_str())
                .or_default()
                .push(gc.jid.as_str());
        }
        m
    }

    /// Ordered channel edges: implicit `default` first, then each configured channel.
    pub fn channel_rows(&self) -> Vec<ChannelRow> {
        let routed = self.routed();
        let mut rows = vec![ChannelRow {
            label: "default".into(),
            url: self.agent_inbound_url.clone(),
            token: "whatsapp_webhook_token".into(),
            serves: "DMs + unmapped groups".into(),
            implicit: true,
            sink: self.debug_sink,
        }];
        for c in &self.channels {
            let n_groups = routed.get(c.label.as_str()).map_or(0, Vec::len);
            let token = match &c.token_secret {
                Some(t) => t.rsplit('/').next().unwrap_or(t).to_string(),
                None => "whatsapp_webhook_token (shared)".into(),
            };
            rows.push(ChannelRow {
                serves: format!("{n_groups} group(s)"),
                label: c.label.clone(),
                url: c.url.clone().unwrap_or_default(),
                token,
                implicit: false,
                sink: false,
            });
        }
        rows
    }

    /// All (jid, channel, admitted-via) routing rows: channel-map entries then allowlist-only groups.
    pub fn routing_rows(&self) -> Vec<(String, String, &'static str)> {
        let mut seen = Vec::new();
        let mut out = Vec::new();
        for gc in &self.group_channels {
            out.push((gc.jid.clone(), gc.channel.clone(), "channel map"));
            seen.push(gc.jid.clone());
        }
        for jid in &self.group_allow {
            if !seen.contains(jid) {
                out.push((jid.clone(), "default".into(), "allowlist"));
            }
        }
        out
    }

    /// Structurally-derived health signals — no clock, no guessing beyond the config.
    pub fn health(&self) -> Vec<(Sev, String)> {
        let mut out = Vec::new();
        if self.debug_sink {
            out.push((
                Sev::Crit,
                "Debug sink is ON — inbound messages are dropped; nothing reaches the agents."
                    .into(),
            ));
        }
        if self.dm_policy == "allowlist" && self.dm_allow.is_empty() {
            out.push((
                Sev::Warn,
                "DM allowlist is empty — no direct message is admitted yet.".into(),
            ));
        }
        if self.dm_policy == "open" {
            out.push((
                Sev::Warn,
                "DM policy is OPEN — any number can reach the agent.".into(),
            ));
        }
        if self.group_policy == "open" {
            out.push((
                Sev::Warn,
                "Group policy is OPEN — any group the number is in is admitted.".into(),
            ));
        }
        if !self.require_mention {
            out.push((
                Sev::Info,
                "require_mention is off — the bot answers every group message, not only when addressed."
                    .into(),
            ));
        }
        let defined = self.defined_labels();
        for gc in &self.group_channels {
            if !defined.contains(&gc.channel.as_str()) {
                out.push((
                    Sev::Crit,
                    format!(
                        "Group {} routes to channel '{}', which is not defined in channels.",
                        gc.jid, gc.channel
                    ),
                ));
            }
            if self.group_allow.contains(&gc.jid) {
                out.push((
                    Sev::Info,
                    format!(
                        "Group {} is in both the allowlist and the channel map — the allowlist entry is redundant (mapped groups are admitted).",
                        gc.jid
                    ),
                ));
            }
        }
        if self.in_fleet && !self.has_config {
            out.push((
                Sev::Warn,
                "Listed in the fleet index but has no tenant config file.".into(),
            ));
        }
        if self.has_config && !self.in_fleet {
            out.push((
                Sev::Info,
                "Hand-built tenant — present as a config file but not in the fleet index.".into(),
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------- load

impl Fleet {
    /// Read and merge the fleet index and every tenant file under `deploy_dir`.
    pub fn load(deploy_dir: &Path) -> io::Result<Fleet> {
        let fleet_path = deploy_dir.join("fleet.yaml");
        let mut index: BTreeMap<String, FleetEntry> = BTreeMap::new();
        if fleet_path.exists() {
            let idx: FleetIndex =
                serde_yaml::from_str(&fs::read_to_string(&fleet_path)?).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("fleet.yaml: {e}"))
                })?;
            for e in idx.tenants.unwrap_or_default() {
                index.insert(e.id.clone(), e);
            }
        }

        // tenant files
        let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
        let tdir = deploy_dir.join("tenants");
        if tdir.is_dir() {
            for entry in fs::read_dir(&tdir)? {
                let p = entry?.path();
                if p.extension().and_then(|s| s.to_str()) == Some("yaml")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                {
                    files.insert(stem.to_string(), p);
                }
            }
        }

        let mut ids: Vec<String> = index.keys().cloned().collect();
        for k in files.keys() {
            if !ids.contains(k) {
                ids.push(k.clone());
            }
        }
        ids.sort();

        let mut tenants = Vec::new();
        for id in ids {
            let idx = index.get(&id);
            let (cfg, raw, source) = match files.get(&id) {
                Some(p) => {
                    let raw = fs::read_to_string(p)?;
                    let cfg: TenantFile = serde_yaml::from_str(&raw).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", p.display()))
                    })?;
                    (cfg, raw, format!("tenants/{id}.yaml"))
                }
                None => (
                    TenantFile::default(),
                    String::new(),
                    "fleet.yaml".to_string(),
                ),
            };
            tenants.push(merge(&id, idx, cfg, &raw, files.contains_key(&id), source));
        }

        Ok(Fleet {
            tenants,
            deploy_dir: deploy_dir.to_path_buf(),
        })
    }
}

fn merge(
    id: &str,
    idx: Option<&FleetEntry>,
    cfg: TenantFile,
    raw: &str,
    has_config: bool,
    source: String,
) -> Tenant {
    let in_fleet = idx.is_some();
    let (status, status_sev) = match idx.and_then(|e| e.status.as_deref()) {
        Some("active") => ("active".to_string(), Sev::Good),
        Some("provisioning") => ("provisioning".to_string(), Sev::Warn),
        Some("decommissioned") => ("decommissioned".to_string(), Sev::Crit),
        Some(other) => (other.to_string(), Sev::Info),
        None if has_config => ("hand-built".to_string(), Sev::Info),
        None => ("no config".to_string(), Sev::Warn),
    };

    let pol = cfg.policy.unwrap_or_default();
    let pick = |a: Option<String>, b: Option<&String>| a.or_else(|| b.cloned()).unwrap_or_default();

    let secrets = cfg
        .secrets
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| {
                    Some((
                        k.as_str()?.to_string(),
                        v.as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    Tenant {
        id: id.to_string(),
        box_host: pick(cfg.box_host, idx.and_then(|e| e.box_host.as_ref())),
        magicdns: pick(cfg.magicdns, idx.and_then(|e| e.magicdns.as_ref())),
        wa_account: idx.and_then(|e| e.wa_account.clone()).unwrap_or_default(),
        device_jid: cfg.device_jid.unwrap_or_default(),
        gowa_device_id: cfg.gowa_device_id.unwrap_or_default(),
        self_number: cfg.self_number.unwrap_or_default(),
        status,
        status_sev,
        in_fleet,
        has_config,
        dm_policy: pol.dm_policy.unwrap_or_else(|| "off".into()),
        dm_allow: pol.dm_allow.unwrap_or_default(),
        group_policy: pol.group_policy.unwrap_or_else(|| "off".into()),
        group_allow: pol.group_allow.unwrap_or_default(),
        require_mention: pol.require_mention.unwrap_or(true),
        send_rate_per_min: pol.send_rate_per_min.unwrap_or(20),
        channels: cfg.channels.unwrap_or_default(),
        group_channels: cfg.group_channels.unwrap_or_default(),
        agent_inbound_url: cfg.agent_inbound_url.unwrap_or_default(),
        debug_sink: cfg.debug_sink.unwrap_or(false),
        secrets,
        group_labels: parse_group_labels(raw),
        source,
    }
}

/// Best-effort: pull a human name out of an inline comment that sits on the SAME line as a quoted
/// JID, e.g. `- "1203...@g.us"   # "2 baes" — added ...` → `2 baes`. Line-based, so it never bridges
/// into the next line and never matches a commented-out config block (where `#` precedes the jid).
pub fn parse_group_labels(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let Some(q1) = line.find('"') else { continue };
        let rest = &line[q1 + 1..];
        let Some(q2) = rest.find('"') else { continue };
        let jid = &rest[..q2];
        if !jid.contains('@') {
            continue;
        }
        let after = &rest[q2 + 1..];
        let Some(hash) = after.find('#') else {
            continue;
        };
        if !after[..hash].trim().is_empty() {
            continue; // something other than whitespace between the jid and the comment
        }
        let comment = after[hash + 1..].trim();
        let name = if let Some(qs) = comment.strip_prefix('"') {
            match qs.find('"') {
                Some(e) => &qs[..e],
                None => comment,
            }
        } else {
            // first clause before an em/en/hyphen dash separator
            comment
                .split(" — ")
                .next()
                .unwrap_or(comment)
                .split(" – ")
                .next()
                .unwrap_or(comment)
                .split(" - ")
                .next()
                .unwrap_or(comment)
        };
        let name = name.trim().trim_matches('"').to_string();
        if !name.is_empty() {
            out.entry(jid.to_string()).or_insert(name);
        }
    }
    out
}

/// Fleet-wide summary tiles for the header.
pub struct Summary {
    pub tenants: usize,
    pub live: usize,
    pub targets: usize,
    pub groups: usize,
    pub gaps: usize,
}

impl Fleet {
    pub fn summary(&self) -> Summary {
        let mut targets = std::collections::BTreeSet::new();
        let mut groups = 0usize;
        let mut gaps = 0usize;
        let mut live = 0usize;
        for t in &self.tenants {
            if matches!(t.status.as_str(), "active" | "hand-built") {
                live += 1;
            }
            for r in t.channel_rows() {
                if !r.url.is_empty() {
                    targets.insert(r.url);
                }
            }
            groups += t.routing_rows().len();
            gaps += t
                .health()
                .iter()
                .filter(|(s, _)| matches!(s, Sev::Warn | Sev::Crit))
                .count();
        }
        Summary {
            tenants: self.tenants.len(),
            live,
            targets: targets.len(),
            groups,
            gaps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_ignore_commented_out_blocks() {
        // `#` precedes the jid → not an inline label; must not be picked up.
        let raw = "group_allow:\n  - \"120363000000000000@g.us\"\n# group_channels:\n#   - jid: \"120363000000000000@g.us\"\n#     channel: support\n";
        let labels = parse_group_labels(raw);
        assert!(labels.is_empty(), "got {labels:?}");
    }

    #[test]
    fn labels_extract_quoted_and_dashed_names() {
        let raw = "  - \"120363428950046857@g.us\"   # \"Bae Bot Test group 1\" (owner 61425700099)\n  - \"120363408568709901@g.us\"   # \"2 baes\" — added 2026-06-21\n  - \"120363409170508840@g.us\"   # Dashi by Dashlytix — resolved later\n";
        let labels = parse_group_labels(raw);
        assert_eq!(
            labels.get("120363428950046857@g.us").unwrap(),
            "Bae Bot Test group 1"
        );
        assert_eq!(labels.get("120363408568709901@g.us").unwrap(), "2 baes");
        assert_eq!(
            labels.get("120363409170508840@g.us").unwrap(),
            "Dashi by Dashlytix"
        );
    }

    fn tenant_from(yaml: &str) -> Tenant {
        let cfg: TenantFile = serde_yaml::from_str(yaml).unwrap();
        merge("t", None, cfg, yaml, true, "test.yaml".into())
    }

    #[test]
    fn health_flags_debug_sink_and_empty_dm_allowlist() {
        let t = tenant_from("debug_sink: true\npolicy:\n  dm_policy: allowlist\n  dm_allow: []\n");
        let msgs: Vec<_> = t.health().into_iter().collect();
        assert!(
            msgs.iter().any(|(s, _)| *s == Sev::Crit),
            "expected a crit for debug sink"
        );
        assert!(
            msgs.iter()
                .any(|(_, m)| m.contains("DM allowlist is empty"))
        );
    }

    #[test]
    fn health_flags_undefined_channel_reference() {
        let t = tenant_from(
            "channels:\n  - label: dashi\n    url: http://x\ngroup_channels:\n  - jid: \"1@g.us\"\n    channel: ghost\n",
        );
        assert!(
            t.health()
                .iter()
                .any(|(s, m)| *s == Sev::Crit && m.contains("ghost")),
            "expected crit for undefined channel"
        );
    }

    #[test]
    fn channel_rows_include_implicit_default_first() {
        let t = tenant_from(
            "agent_inbound_url: http://agent\nchannels:\n  - label: dashi\n    url: http://dashi\ngroup_channels:\n  - jid: \"1@g.us\"\n    channel: dashi\n",
        );
        let rows = t.channel_rows();
        assert_eq!(rows[0].label, "default");
        assert!(rows[0].implicit);
        assert_eq!(rows[0].url, "http://agent");
        assert_eq!(rows[1].label, "dashi");
        assert_eq!(rows[1].serves, "1 group(s)");
    }
}

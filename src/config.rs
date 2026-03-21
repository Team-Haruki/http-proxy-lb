use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Local listen address, e.g. "127.0.0.1:8080"
    pub listen: String,

    /// Load-balancing mode
    #[serde(default)]
    pub mode: BalanceMode,

    /// How often to re-read the config file (seconds). 0 = disabled.
    #[serde(default = "default_reload_interval")]
    pub reload_interval_secs: u64,

    /// Active health-check parameters
    #[serde(default)]
    pub health_check: HealthCheckConfig,

    /// Domain-based direct/proxy routing policy
    #[serde(default)]
    pub domain_policy: DomainPolicyConfig,

    /// Upstream proxy list
    #[serde(default)]
    pub upstream: Vec<UpstreamConfig>,
}

fn default_reload_interval() -> u64 {
    60
}

// ---------------------------------------------------------------------------
// Balance mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BalanceMode {
    /// Weighted round-robin over online upstreams (default)
    #[default]
    RoundRobin,
    /// Pick the online upstream with the best (lowest) combined score
    Best,
    /// Pick the online upstream with the highest priority (lowest number)
    Priority,
}

// ---------------------------------------------------------------------------
// Domain policy config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DomainPolicyMode {
    /// Always use upstream proxy (default)
    #[default]
    Off,
    /// Listed domains go direct, others use upstream proxy
    Blacklist,
    /// Listed domains use upstream proxy, others go direct
    Whitelist,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainPolicyConfig {
    #[serde(default)]
    pub mode: DomainPolicyMode,
    #[serde(default)]
    pub domains: Vec<String>,
}

impl Default for DomainPolicyConfig {
    fn default() -> Self {
        Self {
            mode: DomainPolicyMode::Off,
            domains: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Health-check config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    /// Seconds between successive active-probe rounds
    #[serde(default = "default_hc_interval")]
    pub interval_secs: u64,
    /// TCP-connect timeout for each probe (seconds)
    #[serde(default = "default_hc_timeout")]
    pub timeout_secs: u64,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_hc_interval(),
            timeout_secs: default_hc_timeout(),
        }
    }
}

fn default_hc_interval() -> u64 {
    30
}
fn default_hc_timeout() -> u64 {
    5
}

// ---------------------------------------------------------------------------
// Upstream config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    /// Proxy URL, e.g. "http://1.2.3.4:8080"
    pub url: String,
    /// Relative weight for round_robin selection (default: 1)
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Priority for priority-mode selection (lower value = higher priority)
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Optional HTTP proxy Basic-Auth username
    pub username: Option<String>,
    /// Optional HTTP proxy Basic-Auth password
    pub password: Option<String>,
}

fn default_weight() -> u32 {
    1
}

fn default_priority() -> u32 {
    100
}

impl UpstreamConfig {
    /// Returns the `Proxy-Authorization: Basic …` header value, if auth is configured.
    pub fn proxy_auth_header(&self) -> Option<String> {
        use base64::Engine;
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
                Some(format!("Basic {encoded}"))
            }
            _ => None,
        }
    }

    /// Parse `host` and `port` from the upstream URL.
    pub fn host_port(&self) -> Result<(String, u16)> {
        let stripped = self
            .url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/');
        let mut iter = stripped.splitn(2, ':');
        let host = iter
            .next()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Invalid upstream URL: {}", self.url))?
            .to_string();
        let port: u16 = iter.next().and_then(|p| p.parse().ok()).unwrap_or(8080);
        Ok((host, port))
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

pub fn load_config(path: &str) -> Result<Config> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read config file: {path}"))?;
    let config: Config = yaml_serde::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {path}"))?;
    Ok(config)
}

/// Returns the mtime of `path`, used to detect file changes for hot reload.
pub fn file_mtime(path: &str) -> Option<SystemTime> {
    Path::new(path).metadata().ok()?.modified().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_port_plain() {
        let cfg = UpstreamConfig {
            url: "http://proxy.example.com:8080".to_string(),
            weight: 1,
            priority: 100,
            username: None,
            password: None,
        };
        let (host, port) = cfg.host_port().unwrap();
        assert_eq!(host, "proxy.example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn test_host_port_default_port() {
        let cfg = UpstreamConfig {
            url: "http://proxy.example.com".to_string(),
            weight: 1,
            priority: 100,
            username: None,
            password: None,
        };
        let (host, port) = cfg.host_port().unwrap();
        assert_eq!(host, "proxy.example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn test_proxy_auth_header() {
        let cfg = UpstreamConfig {
            url: "http://p:1".to_string(),
            weight: 1,
            priority: 100,
            username: Some("alice".to_string()),
            password: Some("s3cr3t".to_string()),
        };
        let hdr = cfg.proxy_auth_header().unwrap();
        assert!(hdr.starts_with("Basic "));
        // base64("alice:s3cr3t")
        use base64::Engine;
        let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cr3t");
        assert_eq!(hdr, format!("Basic {expected}"));
    }

    #[test]
    fn test_proxy_auth_header_none() {
        let cfg = UpstreamConfig {
            url: "http://p:1".to_string(),
            weight: 1,
            priority: 100,
            username: None,
            password: None,
        };
        assert!(cfg.proxy_auth_header().is_none());
    }

    #[test]
    fn test_parse_yaml() {
        let yaml = r#"
listen: "127.0.0.1:9090"
mode: best
upstream:
  - url: "http://a:1"
    weight: 2
    priority: 10
  - url: "http://b:2"
"#;
        let cfg: Config = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:9090");
        assert_eq!(cfg.mode, BalanceMode::Best);
        assert_eq!(cfg.upstream.len(), 2);
        assert_eq!(cfg.upstream[0].weight, 2);
        assert_eq!(cfg.upstream[0].priority, 10);
        assert_eq!(cfg.upstream[1].weight, 1); // default
        assert_eq!(cfg.upstream[1].priority, 100); // default
    }

    #[test]
    fn test_parse_yaml_defaults() {
        let yaml = r#"
listen: "0.0.0.0:8080"
upstream: []
"#;
        let cfg: Config = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(cfg.mode, BalanceMode::RoundRobin);
        assert_eq!(cfg.reload_interval_secs, 60);
        assert_eq!(cfg.health_check.interval_secs, 30);
        assert_eq!(cfg.health_check.timeout_secs, 5);
        assert_eq!(cfg.domain_policy.mode, DomainPolicyMode::Off);
        assert!(cfg.domain_policy.domains.is_empty());
    }

    #[test]
    fn test_parse_yaml_domain_policy() {
        let yaml = r#"
listen: "0.0.0.0:8080"
domain_policy:
  mode: blacklist
  domains:
    - "example.com"
    - "internal.local"
upstream: []
"#;
        let cfg: Config = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(cfg.domain_policy.mode, DomainPolicyMode::Blacklist);
        assert_eq!(cfg.domain_policy.domains.len(), 2);
        assert_eq!(cfg.domain_policy.domains[0], "example.com");
    }
}

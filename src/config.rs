use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::HashSet, path::Path};

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip)]
    pub deprecated_keys: Vec<String>,
    pub general: General,
    pub logging: Logging,
    pub miniserver: Miniserver,
    pub paths: Paths,
    pub telegraf: Telegraf,
    pub filters: Filters,
    pub processing: Processing,
    pub monitoring: Monitoring,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub grabber: bool,
    pub round_floats: bool,
    pub rounding_precision: u32,
    pub grabber_interval: u64,
    pub grabber_concurrency: usize,
    pub config_refresh_interval: u64,
}
impl Default for General {
    fn default() -> Self {
        Self {
            grabber: true,
            round_floats: false,
            rounding_precision: 5,
            grabber_interval: 300,
            grabber_concurrency: 10,
            config_refresh_interval: 300,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: String,
}
impl Default for Logging {
    fn default() -> Self {
        Self {
            level: "INFO".into(),
        }
    }
}

// Deliberately no Debug implementation: configurations contain credentials.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Miniserver {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub visu_password: Option<String>,
    pub max_reconnect_attempts: u32,
    pub tls_verify: bool,
    pub request_timeout: u64,
}
impl Default for Miniserver {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 80,
            user: "admin".into(),
            password: "admin".into(),
            visu_password: None,
            max_reconnect_attempts: 0,
            tls_verify: false,
            request_timeout: 30,
        }
    }
}
impl Miniserver {
    pub fn base_url(&self) -> String {
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            443 => format!("https://{host}"),
            80 => format!("http://{host}"),
            p => format!("http://{host}:{p}"),
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub data_dir: String,
}
impl Default for Paths {
    fn default() -> Self {
        Self {
            data_dir: "data".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum Protocol {
    Udp,
    Tcp,
    Execd,
    Mqtt,
}
impl TryFrom<String> for Protocol {
    type Error = String;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        match value.to_ascii_lowercase().as_str() {
            "udp" => Ok(Self::Udp),
            "tcp" => Ok(Self::Tcp),
            "execd" => Ok(Self::Execd),
            "mqtt" => Ok(Self::Mqtt),
            _ => Err("telegraf.protocol must be udp, tcp, execd or mqtt".into()),
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telegraf {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
    pub mqtt: Mqtt,
    pub max_retries: i32,
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub udp_max_bytes: usize,
    pub write_timeout: u64,
    pub shutdown_timeout: u64,
}
impl Default for Telegraf {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8086,
            protocol: Protocol::Udp,
            mqtt: Mqtt::default(),
            max_retries: 100,
            queue_capacity: 65536,
            batch_size: 512,
            udp_max_bytes: 65507,
            write_timeout: 10,
            shutdown_timeout: 15,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mqtt {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topic: String,
    pub client_id: String,
}
impl Default for Mqtt {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 1883,
            username: None,
            password: None,
            topic: "loxone/metrics".into(),
            client_id: "loxinflux".into(),
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Filters {
    pub type_blacklist: HashSet<String>,
    pub websocket: Filter,
    pub grabber: Filter,
}
#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub type_blacklist: HashSet<String>,
    pub type_whitelist: HashSet<String>,
    pub uuid_blacklist: HashSet<String>,
    pub uuid_whitelist: HashSet<String>,
}
impl Filter {
    pub fn includes(&self, kind: &str, uuid: &str) -> bool {
        let types = if self.type_whitelist.is_empty() {
            !self.type_blacklist.contains(kind)
        } else {
            self.type_whitelist.contains(kind)
        };
        types
            && if self.uuid_whitelist.is_empty() {
                !self.uuid_blacklist.contains(uuid)
            } else {
                self.uuid_whitelist.contains(uuid)
            }
    }
    fn normalize(&mut self) {
        self.type_blacklist = self
            .type_blacklist
            .drain()
            .map(|s| s.to_uppercase())
            .collect();
        self.type_whitelist = self
            .type_whitelist
            .drain()
            .map(|s| s.to_uppercase())
            .collect();
        self.uuid_blacklist = self
            .uuid_blacklist
            .drain()
            .map(|s| normalize_uuid(&s))
            .collect();
        self.uuid_whitelist = self
            .uuid_whitelist
            .drain()
            .map(|s| normalize_uuid(&s))
            .collect();
    }
}
pub fn normalize_uuid(s: &str) -> String {
    s.trim().trim_start_matches("U:").to_ascii_lowercase()
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Processing {
    pub only_on_change: bool,
    pub heartbeat_interval: u64,
    pub drop_empty: bool,
    pub drop_placeholders: bool,
    pub parse_numeric_units: bool,
    pub separate_text_fields: bool,
    pub drop_redundant_default: bool,
}
impl Default for Processing {
    fn default() -> Self {
        Self {
            only_on_change: false,
            heartbeat_interval: 3600,
            drop_empty: true,
            drop_placeholders: true,
            parse_numeric_units: true,
            separate_text_fields: true,
            drop_redundant_default: false,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Monitoring {
    pub enabled: bool,
    pub listen: String,
}
impl Default for Monitoring {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "0.0.0.0:9108".into(),
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
        Self::parse(&raw)
    }
    pub fn parse(raw: &str) -> Result<Self> {
        // Do not render the TOML source in errors: it may contain passwords.
        let mut document: toml::Value = toml::from_str(raw)
            .map_err(|e| anyhow::anyhow!("Invalid TOML config: {}", e.message()))?;
        // Only relax sections that the Python loader already treated permissively.
        // Never reinterpret ignored keys as active settings or log their values.
        let mut deprecated_keys = Vec::new();
        for (section, allowed) in [
            (
                "",
                &[
                    "general",
                    "logging",
                    "miniserver",
                    "paths",
                    "telegraf",
                    "filters",
                    "processing",
                    "monitoring",
                ][..],
            ),
            (
                "telegraf",
                &[
                    "host",
                    "port",
                    "protocol",
                    "mqtt",
                    "max_retries",
                    "queue_capacity",
                    "batch_size",
                    "udp_max_bytes",
                    "write_timeout",
                    "shutdown_timeout",
                ][..],
            ),
            ("filters", &["type_blacklist", "websocket", "grabber"][..]),
        ] {
            let value = if section.is_empty() {
                Some(&mut document)
            } else {
                document.get_mut(section)
            };
            if let Some(table) = value.and_then(toml::Value::as_table_mut) {
                table.retain(|key, _| {
                    if allowed.contains(&key) {
                        return true;
                    }
                    deprecated_keys.push(if section.is_empty() {
                        key.to_owned()
                    } else {
                        format!("{section}.{key}")
                    });
                    false
                });
            }
        }
        let mut config: Self = document.try_into().map_err(|e: toml::de::Error| {
            anyhow::anyhow!("Invalid TOML config: {}", e.message())
        })?;
        config.deprecated_keys = deprecated_keys;
        config.filters.type_blacklist = config
            .filters
            .type_blacklist
            .drain()
            .map(|s| s.to_uppercase())
            .collect();
        config.filters.websocket.normalize();
        config.filters.grabber.normalize();
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=65536).contains(&self.telegraf.batch_size),
            "telegraf.batch_size must be 1..65536"
        );
        ensure!(
            (1..=1048576).contains(&self.telegraf.queue_capacity),
            "telegraf.queue_capacity must be 1..1048576"
        );
        ensure!(
            (512..=65507).contains(&self.telegraf.udp_max_bytes),
            "telegraf.udp_max_bytes must be 512..65507"
        );
        ensure!(
            self.general.rounding_precision <= 15,
            "rounding_precision must be 0..15"
        );
        ensure!(
            self.general.grabber_interval > 0,
            "grabber_interval must be positive"
        );
        ensure!(
            (1..=128).contains(&self.general.grabber_concurrency),
            "grabber_concurrency must be 1..128"
        );
        ensure!(
            self.miniserver.request_timeout > 0
                && self.telegraf.write_timeout > 0
                && self.telegraf.shutdown_timeout > 0,
            "Timeouts must be positive"
        );
        validate_host("miniserver.host", &self.miniserver.host)?;
        ensure!(self.miniserver.port > 0, "miniserver.port must be positive");
        match self.telegraf.protocol {
            Protocol::Udp | Protocol::Tcp => {
                validate_host("telegraf.host", &self.telegraf.host)?;
                ensure!(self.telegraf.port > 0, "telegraf.port must be positive");
            }
            Protocol::Mqtt => {
                let mqtt = &self.telegraf.mqtt;
                validate_host("telegraf.mqtt.host", &mqtt.host)?;
                ensure!(mqtt.port > 0, "telegraf.mqtt.port must be positive");
                ensure!(
                    !mqtt.topic.is_empty() && !mqtt.topic.contains(['#', '+']),
                    "Invalid MQTT publish topic"
                );
                ensure!(
                    !mqtt.client_id.is_empty(),
                    "MQTT client_id must not be empty"
                );
                for (name, value) in [
                    ("topic", mqtt.topic.as_str()),
                    ("client_id", mqtt.client_id.as_str()),
                    ("username", mqtt.username.as_deref().unwrap_or("")),
                ] {
                    ensure!(
                        value.len() <= u16::MAX as usize && !value.contains('\0'),
                        "telegraf.mqtt.{name} must fit 65535 UTF-8 bytes without NUL"
                    );
                }
                ensure!(
                    mqtt.password.as_deref().unwrap_or("").len() <= u16::MAX as usize,
                    "telegraf.mqtt.password must fit 65535 bytes"
                );
            }
            Protocol::Execd => {}
        }
        if self.monitoring.enabled {
            self.monitoring
                .listen
                .parse::<std::net::SocketAddr>()
                .context("monitoring.listen must be IP:port")?;
        }
        Ok(())
    }
}

fn validate_host(name: &str, host: &str) -> Result<()> {
    ensure!(
        !host.is_empty()
            && !host.chars().any(|c| c.is_whitespace() || c.is_control())
            && !host.contains(['/', '?', '#', '@']),
        "{name} must be a host without whitespace, credentials, URL scheme or path"
    );
    if host.contains([':', '[', ']']) {
        let address = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host);
        ensure!(
            address.parse::<std::net::Ipv6Addr>().is_ok(),
            "{name} must be a hostname or IPv6 address; configure the port separately"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_template_loads_without_changes() {
        let c = Config::parse(include_str!("../tests/fixtures/legacy_config.toml")).unwrap();
        assert!(c.general.round_floats);
        assert_eq!(c.telegraf.protocol, Protocol::Udp);
        assert!(c.filters.type_blacklist.contains("VIRTUALOUTCMD"));
        assert!(!c.processing.only_on_change);
        assert_eq!(c.telegraf.udp_max_bytes, 65507);
        assert_eq!(
            Config::parse("[telegraf]\nprotocol='TCP'")
                .unwrap()
                .telegraf
                .protocol,
            Protocol::Tcp
        );
    }
    #[test]
    fn deprecated_legacy_keys_are_ignored_without_changing_rounding() {
        let c = Config::parse("unused='private-value'\n[general]\nround_floats=false\nrounding_precision=7\n[telegraf]\nround_floats=true\nround_decimals=2\n[filters]\nunused=true").unwrap();
        assert!(!c.general.round_floats);
        assert_eq!(c.general.rounding_precision, 7);
        assert_eq!(
            c.deprecated_keys,
            [
                "unused",
                "telegraf.round_decimals",
                "telegraf.round_floats",
                "filters.unused"
            ]
        );
        assert!(!format!("{:?}", c.deprecated_keys).contains("private-value"));
    }
    #[test]
    fn whitelist_overrides_blacklist_with_parent_checked_by_caller() {
        let c = Config::parse("[filters.websocket]\ntype_blacklist=['sensor']\ntype_whitelist=['sensor']\nuuid_whitelist=['U:AB']").unwrap();
        assert!(c.filters.websocket.includes("SENSOR", "ab"));
        assert!(!c.filters.websocket.includes("SENSOR", "cd"));
    }
    #[test]
    fn rejects_typos_and_zero_intervals() {
        for s in [
            "[general]\ngrabber_interval=0",
            "[general]\nround_float=true",
            "[telegraf]\nprotocol='typo'",
        ] {
            assert!(Config::parse(s).is_err());
        }
    }
    #[test]
    fn urls_match_legacy_ports_and_support_ipv6() {
        let mut m = Miniserver {
            host: "::1".into(),
            ..Default::default()
        };
        assert_eq!(m.base_url(), "http://[::1]");
        m.port = 443;
        assert_eq!(m.base_url(), "https://[::1]");
        m.port = 8080;
        assert_eq!(m.base_url(), "http://[::1]:8080");
    }
    #[test]
    fn active_endpoints_are_validated_before_connecting() {
        for host in [
            "",
            " ",
            "host\nname",
            "http://host",
            "host:1883",
            "user@host",
            "host?x=y",
            "[broken]",
        ] {
            let mut config = Config::default();
            config.miniserver.host = host.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("miniserver.host")
            );
            config.miniserver.host = "::1".into();
            config.telegraf.host = host.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("telegraf.host")
            );
            config.telegraf.protocol = Protocol::Mqtt;
            config.telegraf.mqtt.host = host.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("telegraf.mqtt.host")
            );
        }
        for host in ["localhost", "192.168.1.1", "::1", "[::1]"] {
            let mut config = Config::default();
            config.miniserver.host = host.into();
            config.validate().unwrap();
        }
    }
    #[test]
    fn mqtt_fields_obey_wire_byte_lengths_without_disclosing_credentials() {
        let mut config = Config::default();
        config.telegraf.protocol = Protocol::Mqtt;
        config.telegraf.mqtt.topic = "ü".repeat(32768);
        assert!(config.validate().unwrap_err().to_string().contains("topic"));
        config.telegraf.mqtt.topic = "ü".repeat(32767);
        config.validate().unwrap();
        config.telegraf.mqtt.client_id = "bad\0id".into();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("client_id")
        );
        config.telegraf.mqtt.client_id = "id".into();
        config.telegraf.mqtt.username = Some("private\0name".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("username") && !error.contains("private"));
        config.telegraf.mqtt.username = Some("".into());
        config.telegraf.mqtt.password = Some("secret".repeat(12000));
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("password") && !error.contains("secret"));
    }
    #[test]
    fn inactive_output_settings_do_not_block_legacy_configurations() {
        let mut config = Config::default();
        config.telegraf.mqtt.host.clear();
        config.telegraf.mqtt.port = 0;
        config.telegraf.mqtt.topic.clear();
        config.telegraf.mqtt.client_id.clear();
        config.validate().unwrap();
        config.telegraf.protocol = Protocol::Execd;
        config.telegraf.host.clear();
        config.telegraf.port = 0;
        config.validate().unwrap();
    }
}

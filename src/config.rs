use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::cli::ConfigAction;
use crate::error::NutError;
use crate::host::Host;
use crate::profiles::ProfileId;

const CONFIG_FILE: &str = "nut-cli.toml";
const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub schema_version: u32,
    pub host: HostSettings,
    pub ups: UpsSettings,
    #[serde(default)]
    pub protection: ProtectionSettings,
    #[serde(default)]
    pub mounts: Vec<MountRequirement>,
    #[serde(default)]
    pub notifications: Option<NotificationSettings>,
}

impl HostConfig {
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(contents).map_err(|_| ConfigError::InvalidSchema)?;
        if config.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(config.schema_version));
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSettings {
    pub name: String,
    #[serde(default)]
    pub board_profile: Option<ProfileId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsSettings {
    pub name: String,
    pub profile: ProfileId,
    #[serde(default = "default_upsd_address")]
    pub upsd_address: String,
    #[serde(default = "default_upsd_port")]
    pub upsd_port: u16,
}

fn default_upsd_address() -> String {
    "127.0.0.1".to_owned()
}

const fn default_upsd_port() -> u16 {
    3493
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectionSettings {
    pub poweroff_wait: Option<DurationValue>,
    pub on_battery_shutdown: Option<DurationValue>,
    pub low_battery_runtime: Option<DurationValue>,
    pub battery_replaced: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountRequirement {
    pub path: PathBuf,
    #[serde(default)]
    pub nofail: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationSettings {
    #[serde(default)]
    pub recipients: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DurationValue(pub Duration);

impl DurationValue {
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let (digits, unit) = input.trim().split_at(
            input
                .trim()
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(input.trim().len()),
        );
        if digits.is_empty() || unit.is_empty() {
            return Err(ConfigError::InvalidDuration(input.to_owned()));
        }
        let value = digits
            .parse::<u64>()
            .map_err(|_| ConfigError::InvalidDuration(input.to_owned()))?;
        let multiplier = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 60 * 60,
            _ => return Err(ConfigError::InvalidDuration(input.to_owned())),
        };
        value
            .checked_mul(multiplier)
            .map(Duration::from_secs)
            .map(Self)
            .ok_or_else(|| ConfigError::InvalidDuration(input.to_owned()))
    }

    pub fn as_duration(self) -> Duration {
        self.0
    }
}

impl std::fmt::Display for DurationValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = self.0.as_secs();
        if seconds >= 3600 {
            write!(
                formatter,
                "{}h {:02}m",
                seconds / 3600,
                (seconds % 3600) / 60
            )
        } else if seconds >= 60 && !seconds.is_multiple_of(60) {
            write!(formatter, "{}m {:02}s", seconds / 60, seconds % 60)
        } else if seconds >= 60 {
            write!(formatter, "{}m", seconds / 60)
        } else {
            write!(formatter, "{seconds}s")
        }
    }
}

impl Serialize for DurationValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let seconds = self.0.as_secs();
        let value = if seconds != 0 && seconds.is_multiple_of(3600) {
            format!("{}h", seconds / 3600)
        } else if seconds != 0 && seconds.is_multiple_of(60) {
            format!("{}m", seconds / 60)
        } else {
            format!("{seconds}s")
        };
        serializer.serialize_str(&value)
    }
}

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("cannot determine a home directory; set NUT_CONFIG or XDG_CONFIG_HOME")]
    NoHome,
    #[error("configuration file does not exist: {0}")]
    NotFound(PathBuf),
    #[error("cannot read configuration file: {0}")]
    ReadFailure(PathBuf),
    #[error("configuration file is not valid for schema version 1")]
    InvalidSchema,
    #[error("unsupported config schema version {0}; this build supports version 1")]
    UnsupportedSchema(u32),
    #[error("invalid duration {0:?}; use an integer followed by s, m, or h")]
    InvalidDuration(String),
}

pub fn config_path<H: Host>(
    host: &H,
    override_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = host.env("NUT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = host.env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("nut").join(CONFIG_FILE));
    }
    host.home_dir()
        .map(|home| home.join(".config").join("nut").join(CONFIG_FILE))
        .ok_or(ConfigError::NoHome)
}

pub fn load_config<H: Host>(
    host: &H,
    override_path: Option<&Path>,
) -> Result<(PathBuf, HostConfig), ConfigError> {
    let path = config_path(host, override_path)?;
    let bytes = host
        .read_file(&path)
        .map_err(|_| ConfigError::ReadFailure(path.clone()))?
        .ok_or_else(|| ConfigError::NotFound(path.clone()))?;
    let contents = std::str::from_utf8(&bytes).map_err(|_| ConfigError::InvalidSchema)?;
    let config = HostConfig::parse(contents)?;
    Ok((path, config))
}

pub fn run<H: Host, W: Write>(
    action: Option<ConfigAction>,
    override_path: Option<&Path>,
    json: bool,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    match action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Path => {
            let path = config_path(host, override_path)
                .map_err(|error| NutError::operational(error.to_string()))?;
            if json {
                serde_json::to_writer_pretty(
                    &mut *stdout,
                    &serde_json::json!({"schema_version": 1, "path": path}),
                )
                .map_err(|error| {
                    NutError::operational(format!("cannot write JSON output: {error}"))
                })?;
                writeln!(stdout).map_err(|error| {
                    NutError::operational(format!("cannot write output: {error}"))
                })?;
            } else {
                writeln!(stdout, "{}", path.display()).map_err(|error| {
                    NutError::operational(format!("cannot write output: {error}"))
                })?;
            }
        }
        ConfigAction::Show => {
            let (_, config) = load_config(host, override_path)
                .map_err(|error| NutError::operational(error.to_string()))?;
            if json {
                serde_json::to_writer_pretty(&mut *stdout, &config).map_err(|error| {
                    NutError::operational(format!("cannot write JSON output: {error}"))
                })?;
                writeln!(stdout).map_err(|error| {
                    NutError::operational(format!("cannot write output: {error}"))
                })?;
            } else {
                render_summary(&config, stdout)?;
            }
        }
    }
    Ok(())
}

fn render_summary<W: Write>(config: &HostConfig, stdout: &mut W) -> Result<(), NutError> {
    writeln!(stdout, "Host             {}", config.host.name)
        .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    writeln!(
        stdout,
        "UPS              {} ({})",
        config.ups.name, config.ups.profile
    )
    .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    writeln!(
        stdout,
        "upsd             {}:{}",
        config.ups.upsd_address, config.ups.upsd_port
    )
    .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    if let Some(profile) = &config.host.board_profile {
        writeln!(stdout, "Board profile    {profile}")
            .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    }
    if let Some(wait) = config.protection.poweroff_wait {
        writeln!(stdout, "Poweroff wait    {wait}")
            .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    }
    if let Some(delay) = config.protection.on_battery_shutdown {
        writeln!(stdout, "Shutdown delay   {delay}")
            .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    }
    if !config.mounts.is_empty() {
        writeln!(stdout, "Protected mounts {}", config.mounts.len())
            .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    }
    if let Some(notifications) = &config.notifications {
        writeln!(
            stdout,
            "Notifications    {} recipient(s)",
            notifications.recipients.len()
        )
        .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;

    const CONFIG: &str = r#"
schema_version = 1

[host]
name = "athena"
board_profile = "asus-prime-trx40-pro"

[ups]
name = "cp1500"
profile = "cyberpower-cp1500pfclcd"
upsd_address = "127.0.0.1"

[protection]
poweroff_wait = "45m"
on_battery_shutdown = "180s"

[[mounts]]
path = "/srv/media"
nofail = true

[notifications]
recipients = ["ups@example.invalid"]
"#;

    #[test]
    fn parses_host_settings_and_policy_without_secrets() {
        let config = HostConfig::parse(CONFIG).unwrap();
        assert_eq!(config.host.name, "athena");
        assert_eq!(config.ups.upsd_port, 3493);
        assert_eq!(config.protection.poweroff_wait.unwrap().to_string(), "45m");
        let output = serde_json::to_string(&config).unwrap();
        assert!(!output.contains("password"));
        assert!(
            HostConfig::parse(&CONFIG.replace("schema_version = 1", "schema_version = 2")).is_err()
        );
    }

    #[test]
    fn rejects_unknown_fields_including_credentials() {
        let input = CONFIG.replace("[host]", "password = \"never-load-this\"\n\n[host]");
        assert_eq!(
            HostConfig::parse(&input).unwrap_err(),
            ConfigError::InvalidSchema
        );
    }

    #[test]
    fn duration_values_parse_and_serialize_as_short_units() {
        assert_eq!(
            DurationValue::parse("25m").unwrap().as_duration(),
            Duration::from_secs(1500)
        );
        assert_eq!(DurationValue::parse("4s").unwrap().to_string(), "4s");
        let settings = ProtectionSettings {
            poweroff_wait: Some(DurationValue(Duration::from_secs(3600))),
            ..ProtectionSettings::default()
        };
        assert!(
            toml::to_string(&settings)
                .unwrap()
                .contains("poweroff_wait = \"1h\"")
        );
        assert!(DurationValue::parse("2d").is_err());
    }

    #[test]
    fn explicit_and_environment_config_paths_take_precedence() {
        let host = FakeHost::default()
            .with_env("HOME", "/home/test")
            .with_env("NUT_CONFIG", "/tmp/nut.toml");
        assert_eq!(
            config_path(&host, None).unwrap(),
            PathBuf::from("/tmp/nut.toml")
        );
        assert_eq!(
            config_path(&host, Some(Path::new("/etc/nut/host.toml"))).unwrap(),
            PathBuf::from("/etc/nut/host.toml")
        );
    }

    #[test]
    fn loads_and_shows_config_through_host_trait() {
        let path = PathBuf::from("/hosts/test/nut-cli.toml");
        let host = FakeHost::default().with_file(&path, CONFIG.as_bytes().to_vec());
        let (_, config) = load_config(&host, Some(&path)).unwrap();
        assert_eq!(config.host.name, "athena");
    }
}

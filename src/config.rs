use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::cli::ConfigAction;
use crate::duration::DurationValue;
use crate::error::NutError;
use crate::host::Host;
use crate::profiles::{
    ProfileCatalog, ProfileId, UpsProfile, UsbId, discover_usb_ids, load_catalog, read_dmi,
};
use crate::render::{self, RenderedFiles};

const DEFAULT_CONFIG_PATH: &str = "/etc/nut-cli/nut.toml";
const CONFIG_SCHEMA: u32 = 1;
const ALLOWED_EXTRA_KEYS: &[&str] = &[
    "pollonly",
    "onlinedischarge",
    "maxretry",
    "retrydelay",
    "serial",
    "bus",
    "device",
    "vendor",
    "product",
    "sdtype",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    Flag,
    Env,
    Default,
}

impl ConfigSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Flag => "--config",
            Self::Env => "NUT_CONFIG",
            Self::Default => "default",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub schema: u32,
    pub host: HostSettings,
    #[serde(default)]
    pub ups: BTreeMap<String, UpsHostEntry>,
    pub shutdown: ShutdownSettings,
    pub notify: NotifySettings,
    #[serde(default)]
    pub boot: BootSettings,
    #[serde(default)]
    pub operators: OperatorsSettings,
    #[serde(default)]
    pub battery: Option<BatterySettings>,
}

impl HostConfig {
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(contents)
            .map_err(|error| ConfigError::InvalidSchema(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.schema != CONFIG_SCHEMA {
            return Err(ConfigError::UnsupportedSchema(self.schema));
        }
        self.ups_entry()?;
        for (name, entry) in &self.ups {
            validate_ups_name(name)?;
            for key in entry.extra.keys() {
                if !ALLOWED_EXTRA_KEYS.contains(&key.as_str()) {
                    return Err(ConfigError::InvalidExtraKey(key.clone()));
                }
            }
        }
        Ok(())
    }

    pub fn ups_entry(&self) -> Result<(&str, &UpsHostEntry), ConfigError> {
        match self.ups.len() {
            0 => Err(ConfigError::NoUps),
            1 => Ok(self
                .ups
                .iter()
                .next()
                .map(|(name, entry)| (name.as_str(), entry))
                .expect("checked length")),
            _ => Err(ConfigError::MultipleUps(
                self.ups.keys().cloned().collect::<Vec<_>>().join(", "),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSettings {
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsHostEntry {
    pub profile: ProfileId,
    pub description: Option<String>,
    #[serde(default, rename = "match")]
    pub usb_match: UpsMatch,
    pub offdelay: Option<DurationValue>,
    pub ondelay: Option<DurationValue>,
    pub driver: Option<String>,
    pub port: Option<String>,
    #[serde(default, deserialize_with = "deserialize_extra")]
    pub extra: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsMatch {
    pub vendorid: Option<String>,
    pub productid: Option<String>,
    pub serial: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownSettings {
    pub notify_after: DurationValue,
    pub early_after: DurationValue,
    pub poweroff_wait: DurationValue,
    pub shutdown_budget: DurationValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySettings {
    pub from: String,
    pub to: String,
    pub commbad_after: DurationValue,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootSettings {
    #[serde(default)]
    pub nofail_mounts: Vec<PathBuf>,
    pub board_profile: Option<ProfileId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorsSettings {
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub journal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatterySettings {
    pub installed: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    Host,
    Profile,
    Default,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Sourced<T> {
    pub value: T,
    pub source: ValueSource,
}

impl<T> Sourced<T> {
    fn host(value: T) -> Self {
        Self {
            value,
            source: ValueSource::Host,
        }
    }

    fn profile(value: T) -> Self {
        Self {
            value,
            source: ValueSource::Profile,
        }
    }

    fn default_value(value: T) -> Self {
        Self {
            value,
            source: ValueSource::Default,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedConfig {
    pub path: PathBuf,
    pub host: HostSettings,
    pub ups_name: String,
    pub ups: UpsHostEntry,
    pub profile: UpsProfile,
    pub driver: Sourced<String>,
    pub port: Sourced<String>,
    pub description: Sourced<String>,
    pub offdelay: Sourced<DurationValue>,
    pub ondelay: Sourced<DurationValue>,
    pub vendorid: Sourced<String>,
    pub productid: Sourced<String>,
    pub extra: BTreeMap<String, String>,
    pub shutdown: ShutdownSettings,
    pub notify: NotifySettings,
    pub boot: BootSettings,
    pub operators: OperatorsSettings,
    pub battery: Option<BatterySettings>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("cannot determine a configuration path; set --config or NUT_CONFIG")]
    NoHome,
    #[error("configuration file does not exist: {0}")]
    NotFound(PathBuf),
    #[error("cannot read configuration file: {0}")]
    ReadFailure(PathBuf),
    #[error("configuration file is not valid for schema 1: {0}")]
    InvalidSchema(String),
    #[error("unsupported config schema version {0}; this build supports version 1")]
    UnsupportedSchema(u32),
    #[error("v1 supports exactly one UPS; none configured")]
    NoUps,
    #[error("v1 supports exactly one UPS; found {0}")]
    MultipleUps(String),
    #[error("invalid UPS name {0:?}; use a letter followed by letters, digits, '.', '_' or '-'")]
    InvalidUpsName(String),
    #[error(
        "unknown extra ups.conf key {0:?}; allowed: pollonly, onlinedischarge, maxretry, retrydelay, serial, bus, device, vendor, product, sdtype"
    )]
    InvalidExtraKey(String),
    #[error("unknown UPS profile {0}")]
    UnknownProfile(ProfileId),
    #[error("UPS {0} needs offdelay from the profile or host config")]
    MissingOffdelay(String),
    #[error("UPS {0} needs ondelay from the profile or host config")]
    MissingOndelay(String),
    #[error("UPS {0} needs a productid match; profile {1} lists multiple USB ids")]
    AmbiguousUsb(String, ProfileId),
    #[error("invalid USB product id {0:?}; expected four hex digits")]
    InvalidUsbField(String),
    #[error("{0}")]
    Profile(String),
}

pub fn config_path<H: Host>(
    host: &H,
    override_path: Option<&Path>,
) -> Result<(PathBuf, ConfigSource), ConfigError> {
    if let Some(path) = override_path {
        return Ok((path.to_path_buf(), ConfigSource::Flag));
    }
    if let Some(path) = host.env("NUT_CONFIG") {
        return Ok((PathBuf::from(path), ConfigSource::Env));
    }
    Ok((PathBuf::from(DEFAULT_CONFIG_PATH), ConfigSource::Default))
}

pub fn load_config<H: Host>(
    host: &H,
    override_path: Option<&Path>,
) -> Result<(PathBuf, ConfigSource, HostConfig), ConfigError> {
    let (path, source) = config_path(host, override_path)?;
    let bytes = host
        .read_file(&path)
        .map_err(|_| ConfigError::ReadFailure(path.clone()))?
        .ok_or_else(|| ConfigError::NotFound(path.clone()))?;
    let contents = std::str::from_utf8(&bytes)
        .map_err(|_| ConfigError::InvalidSchema(format!("{} is not UTF-8", path.display())))?;
    let config = HostConfig::parse(contents)?;
    Ok((path, source, config))
}

pub fn resolve(
    path: PathBuf,
    config: &HostConfig,
    catalog: &ProfileCatalog,
) -> Result<ResolvedConfig, ConfigError> {
    let (ups_name, entry) = config.ups_entry()?;
    let profile = catalog
        .ups(&entry.profile)
        .cloned()
        .ok_or_else(|| ConfigError::UnknownProfile(entry.profile.clone()))?;
    let driver = match &entry.driver {
        Some(value) => Sourced::host(value.clone()),
        None => Sourced::profile(profile.driver.clone()),
    };
    let port = match &entry.port {
        Some(value) => Sourced::host(value.clone()),
        None if profile.driver == "dummy-ups" => Sourced::default_value("dummy-ups".to_owned()),
        None => Sourced::default_value("auto".to_owned()),
    };
    let description = match &entry.description {
        Some(value) => Sourced::host(value.clone()),
        None => Sourced::profile(format!("{} {}", profile.manufacturer, profile.model)),
    };
    let offdelay = match entry.offdelay {
        Some(value) => Sourced::host(value),
        None => Sourced::profile(
            profile
                .offdelay
                .ok_or_else(|| ConfigError::MissingOffdelay(ups_name.to_owned()))?,
        ),
    };
    let ondelay = match entry.ondelay {
        Some(value) => Sourced::host(value),
        None => Sourced::profile(
            profile
                .ondelay
                .ok_or_else(|| ConfigError::MissingOndelay(ups_name.to_owned()))?,
        ),
    };
    let (vendorid, productid) = resolve_usb(ups_name, entry, &profile)?;
    Ok(ResolvedConfig {
        path,
        host: config.host.clone(),
        ups_name: ups_name.to_owned(),
        ups: entry.clone(),
        profile,
        driver,
        port,
        description,
        offdelay,
        ondelay,
        vendorid,
        productid,
        extra: entry.extra.clone(),
        shutdown: config.shutdown.clone(),
        notify: config.notify.clone(),
        boot: config.boot.clone(),
        operators: config.operators.clone(),
        battery: config.battery.clone(),
    })
}

fn resolve_usb(
    ups_name: &str,
    entry: &UpsHostEntry,
    profile: &UpsProfile,
) -> Result<(Sourced<String>, Sourced<String>), ConfigError> {
    if profile.driver == "dummy-ups" && profile.usb_ids.is_empty() {
        let vendorid = match &entry.usb_match.vendorid {
            Some(value) => Sourced::host(normalize_usb_part(value)?),
            None => Sourced::default_value(String::new()),
        };
        let productid = match &entry.usb_match.productid {
            Some(value) => Sourced::host(normalize_usb_part(value)?),
            None => Sourced::default_value(String::new()),
        };
        return Ok((vendorid, productid));
    }
    let vendorid = if let Some(value) = &entry.usb_match.vendorid {
        Sourced::host(normalize_usb_part(value)?)
    } else if let Some(id) = unique_vendor(profile) {
        Sourced::profile(id.vendor_hex())
    } else if let [id, ..] = profile.usb_ids.as_slice() {
        Sourced::profile(id.vendor_hex())
    } else {
        return Err(ConfigError::InvalidUsbField(format!(
            "UPS {ups_name} needs match.vendorid"
        )));
    };
    let productid = if let Some(value) = &entry.usb_match.productid {
        Sourced::host(normalize_usb_part(value)?)
    } else if profile.usb_ids.len() == 1 {
        Sourced::profile(profile.usb_ids[0].product_hex())
    } else if profile.usb_ids.is_empty() {
        return Err(ConfigError::InvalidUsbField(format!(
            "UPS {ups_name} needs match.productid"
        )));
    } else {
        return Err(ConfigError::AmbiguousUsb(
            ups_name.to_owned(),
            profile.id.clone(),
        ));
    };
    Ok((vendorid, productid))
}

fn unique_vendor(profile: &UpsProfile) -> Option<UsbId> {
    let first = profile.usb_ids.first().copied()?;
    if profile.usb_ids.iter().all(|id| id.vendor == first.vendor) {
        Some(first)
    } else {
        None
    }
}

fn normalize_usb_part(value: &str) -> Result<String, ConfigError> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(ConfigError::InvalidUsbField(value))
    }
}

fn validate_ups_name(name: &str) -> Result<(), ConfigError> {
    let valid = name
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidUpsName(name.to_owned()))
    }
}

fn deserialize_extra<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
    let mut out = BTreeMap::new();
    for (key, value) in values {
        let rendered = match value {
            toml::Value::String(text) => text,
            toml::Value::Integer(number) => number.to_string(),
            toml::Value::Boolean(true) => "enabled".to_owned(),
            toml::Value::Boolean(false) => "disabled".to_owned(),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unsupported extra value for {key}: {other}"
                )));
            }
        };
        out.insert(key, rendered);
    }
    Ok(out)
}

pub fn run<H: Host, W: Write>(
    action: Option<ConfigAction>,
    override_path: Option<&Path>,
    json: bool,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    match action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Path => print_path(host, override_path, json, stdout),
        ConfigAction::Show => print_show(host, override_path, json, stdout),
        ConfigAction::New { output } => new_config(host, output.as_deref(), json, stdout),
        ConfigAction::Render { output } => {
            render_config(host, override_path, output.as_deref(), json, stdout)
        }
        ConfigAction::Diff => diff_config(host, override_path, json, stdout),
    }
}

fn print_path<H: Host, W: Write>(
    host: &H,
    override_path: Option<&Path>,
    json: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let (path, source) = config_path(host, override_path).map_err(config_error)?;
    if json {
        write_json(
            stdout,
            &serde_json::json!({
                "schema_version": 1,
                "path": path,
                "source": source,
            }),
        )?;
    } else {
        writeln!(stdout, "{}", path.display()).map_err(write_error)?;
        writeln!(stdout, "source: {}", source.describe()).map_err(write_error)?;
    }
    Ok(())
}

fn print_show<H: Host, W: Write>(
    host: &H,
    override_path: Option<&Path>,
    json: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let (path, source, config) = load_config(host, override_path).map_err(config_error)?;
    let catalog = load_catalog(host)
        .map_err(|error| NutError::operational(format!("cannot load UPS profiles: {error}")))?;
    let resolved = resolve(path.clone(), &config, &catalog).map_err(config_error)?;
    if json {
        write_json(
            stdout,
            &serde_json::json!({
                "schema_version": 1,
                "path": path,
                "source": source,
                "schema": config.schema,
                "host": config.host,
                "ups": config.ups,
                "shutdown": config.shutdown,
                "notify": config.notify,
                "boot": config.boot,
                "operators": config.operators,
                "battery": config.battery,
            }),
        )?;
    } else {
        render_summary(&resolved, source, stdout)?;
    }
    Ok(())
}

fn new_config<H: Host, W: Write>(
    host: &H,
    output: Option<&Path>,
    json: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let catalog = load_catalog(host)
        .map_err(|error| NutError::operational(format!("cannot load UPS profiles: {error}")))?;
    let scaffold = scaffold_from_discovery(host, &catalog)?;
    if json {
        write_json(
            stdout,
            &serde_json::json!({
                "schema_version": 1,
                "path": output,
                "contents": scaffold,
            }),
        )?;
    }
    if let Some(path) = output {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                NutError::operational(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        std::fs::write(path, &scaffold).map_err(|error| {
            NutError::operational(format!("cannot write {}: {error}", path.display()))
        })?;
        if !json {
            writeln!(stdout, "wrote {}", path.display()).map_err(write_error)?;
        }
    } else if !json {
        write!(stdout, "{scaffold}").map_err(write_error)?;
    }
    Ok(())
}

fn render_config<H: Host, W: Write>(
    host: &H,
    override_path: Option<&Path>,
    output: Option<&Path>,
    json: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let files = render_from_host(host, override_path)?;
    if let Some(dir) = output {
        files.write_to(dir).map_err(|error| {
            NutError::operational(format!("cannot write rendered files: {error}"))
        })?;
        if !json {
            writeln!(stdout, "wrote {}", dir.display()).map_err(write_error)?;
            for (name, _) in files.iter() {
                writeln!(stdout, "  {name}").map_err(write_error)?;
            }
        }
    }
    if json {
        write_json(stdout, &files.json_value())?;
    } else if output.is_none() {
        for (name, contents) in files.iter() {
            writeln!(stdout, "--- {name} ---").map_err(write_error)?;
            write!(stdout, "{contents}").map_err(write_error)?;
            if !contents.ends_with('\n') {
                writeln!(stdout).map_err(write_error)?;
            }
        }
    }
    Ok(())
}

fn diff_config<H: Host, W: Write>(
    host: &H,
    override_path: Option<&Path>,
    json: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let files = render_from_host(host, override_path)?;
    let report = render::diff_live(host, &files)?;
    if json {
        write_json(stdout, &report.json_value())?;
    } else {
        write!(stdout, "{}", report.render_text()).map_err(write_error)?;
    }
    Ok(())
}

fn render_from_host<H: Host>(
    host: &H,
    override_path: Option<&Path>,
) -> Result<RenderedFiles, NutError> {
    let (path, _, config) = load_config(host, override_path).map_err(config_error)?;
    let catalog = load_catalog(host)
        .map_err(|error| NutError::operational(format!("cannot load UPS profiles: {error}")))?;
    let resolved = resolve(path, &config, &catalog).map_err(config_error)?;
    render::render(&resolved).map_err(|error| NutError::operational(error.to_string()))
}

fn scaffold_from_discovery<H: Host>(
    host: &H,
    catalog: &ProfileCatalog,
) -> Result<String, NutError> {
    let hostname = read_hostname(host);
    let usb = discover_usb_ids(host)
        .map_err(|error| NutError::operational(format!("cannot scan USB devices: {error}")))?;
    let mut matched = Vec::new();
    for (_, id) in &usb {
        matched.extend(
            catalog
                .match_usb(*id)
                .into_iter()
                .map(|profile| (profile, *id)),
        );
    }
    matched.sort_by_key(|(profile, _)| profile.id.as_str().to_owned());
    matched.dedup_by_key(|(profile, id)| (profile.id.as_str().to_owned(), *id));
    if matched.len() > 1 {
        let names = matched
            .iter()
            .map(|(profile, id)| format!("{} ({id})", profile.id))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(NutError::operational(format!(
            "v1 supports exactly one UPS; found {names}"
        )));
    }
    let (profile_id, vendorid, productid, description) =
        if let Some((profile, id)) = matched.first() {
            (
                profile.id.as_str().to_owned(),
                id.vendor_hex(),
                id.product_hex(),
                format!("{} {}", profile.manufacturer, profile.model),
            )
        } else {
            (
                "usbhid-generic".to_owned(),
                "TODO".to_owned(),
                "TODO".to_owned(),
                "TODO USB HID UPS".to_owned(),
            )
        };
    let (vendor, board_name) = read_dmi(host)
        .map_err(|error| NutError::operational(format!("cannot read DMI: {error}")))?;
    let board_profile = match (vendor.as_deref(), board_name.as_deref()) {
        (Some(vendor), Some(name)) => catalog
            .match_board(vendor, name)
            .map(|profile| profile.id.as_str().to_owned()),
        _ => None,
    };
    let board_line = match board_profile {
        Some(id) => format!("board_profile = \"{id}\""),
        None => "# board_profile = \"id-from-nut-bios-profile-new\"".to_owned(),
    };
    Ok(format!(
        r#"# Host policy for nut. Hardware knowledge lives in UPS and board profiles.
# This file contains no secrets.
schema = 1

[host]
name = "{hostname}"

[ups.cp1500]
profile = "{profile_id}"
description = "{description}"
port = "auto"

[ups.cp1500.match]
vendorid = "{vendorid}"
productid = "{productid}"

[ups.cp1500.extra]
maxretry = 3
retrydelay = 5

[shutdown]
notify_after = "30s"
early_after = "3m"
poweroff_wait = "30m"
shutdown_budget = "2m"

[notify]
from = "root@example.invalid"
to = "ops@example.invalid"
commbad_after = "60s"

[boot]
nofail_mounts = []
{board_line}

[operators]
users = []
journal = false

# [battery]
# installed = "YYYY-MM-DD"
"#
    ))
}

fn read_hostname<H: Host>(host: &H) -> String {
    let raw = host
        .read_file(Path::new("/etc/hostname"))
        .ok()
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "localhost".to_owned());
    let valid = raw
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
    if valid { raw } else { "localhost".to_owned() }
}

fn render_summary<W: Write>(
    config: &ResolvedConfig,
    source: ConfigSource,
    stdout: &mut W,
) -> Result<(), NutError> {
    writeln!(
        stdout,
        "path             {} ({})",
        config.path.display(),
        source.describe()
    )
    .map_err(write_error)?;
    writeln!(stdout, "Host             {}", config.host.name).map_err(write_error)?;
    writeln!(
        stdout,
        "UPS              {} ({})",
        config.ups_name, config.profile.id
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        "  driver         {} ({})",
        config.driver.value,
        source_label(config.driver.source)
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        "  port           {} ({})",
        config.port.value,
        source_label(config.port.source)
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        "  usb            {}:{} ({})",
        config.vendorid.value,
        config.productid.value,
        source_label(config.productid.source)
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        "  offdelay       {} ({})",
        config.offdelay.value,
        source_label(config.offdelay.source)
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        "  ondelay        {} ({})",
        config.ondelay.value,
        source_label(config.ondelay.source)
    )
    .map_err(write_error)?;
    writeln!(stdout, "notify_after     {}", config.shutdown.notify_after).map_err(write_error)?;
    writeln!(stdout, "early_after      {}", config.shutdown.early_after).map_err(write_error)?;
    writeln!(stdout, "poweroff_wait    {}", config.shutdown.poweroff_wait).map_err(write_error)?;
    writeln!(stdout, "notify from      {}", config.notify.from).map_err(write_error)?;
    writeln!(stdout, "notify to        {}", config.notify.to).map_err(write_error)?;
    if let Some(profile) = &config.boot.board_profile {
        writeln!(stdout, "Board profile    {profile}").map_err(write_error)?;
    }
    if !config.boot.nofail_mounts.is_empty() {
        writeln!(
            stdout,
            "nofail mounts    {}",
            config.boot.nofail_mounts.len()
        )
        .map_err(write_error)?;
    }
    if !config.operators.users.is_empty() {
        writeln!(
            stdout,
            "operators        {}",
            config.operators.users.join(", ")
        )
        .map_err(write_error)?;
    }
    if let Some(battery) = &config.battery {
        writeln!(stdout, "battery          installed {}", battery.installed)
            .map_err(write_error)?;
    }
    Ok(())
}

fn source_label(source: ValueSource) -> &'static str {
    match source {
        ValueSource::Host => "host",
        ValueSource::Profile => "profile",
        ValueSource::Default => "default",
    }
}

fn config_error(error: ConfigError) -> NutError {
    let hint = match &error {
        ConfigError::NotFound(_) => Some("create a host config with nut config new"),
        ConfigError::MultipleUps(_) => Some("v1 supports exactly one [ups.<name>] table"),
        ConfigError::AmbiguousUsb(_, _) => {
            Some("set ups.<name>.match.productid to the live USB product id")
        }
        _ => None,
    };
    let mut nut_error = NutError::operational(error.to_string());
    if let Some(hint) = hint {
        nut_error = nut_error.with_hint(hint);
    }
    nut_error
}

fn write_json<W: Write>(stdout: &mut W, value: &serde_json::Value) -> Result<(), NutError> {
    serde_json::to_writer_pretty(&mut *stdout, value)
        .map_err(|error| NutError::operational(format!("cannot write JSON output: {error}")))?;
    writeln!(stdout).map_err(write_error)
}

fn write_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;

    const CONFIG: &str = r#"
schema = 1

[host]
name = "athena"

[ups.cp1500]
profile = "cyberpower-cp1500pfclcd"
description = "CyberPower CP1500PFCLCD"
port = "auto"

[ups.cp1500.match]
vendorid = "0764"
productid = "0601"

[ups.cp1500.extra]
maxretry = 3
retrydelay = 5

[shutdown]
notify_after = "30s"
early_after = "3m"
poweroff_wait = "30m"
shutdown_budget = "2m"

[notify]
from = "root@example.invalid"
to = "ups@example.invalid"
commbad_after = "60s"

[boot]
nofail_mounts = ["/mnt/poseidon", "/mnt/hades"]
board_profile = "asus-prime-trx40-pro"

[operators]
users = ["bryan"]
journal = false

[battery]
installed = "2026-04-01"
"#;

    #[test]
    fn parses_host_settings_and_policy_without_secrets() {
        let config = HostConfig::parse(CONFIG).unwrap();
        assert_eq!(config.host.name, "athena");
        assert_eq!(config.shutdown.poweroff_wait.to_string(), "30m");
        let output = serde_json::to_string(&config).unwrap();
        assert!(!output.contains("password"));
        assert!(HostConfig::parse(&CONFIG.replace("schema = 1", "schema = 2")).is_err());
    }

    #[test]
    fn rejects_unknown_fields_including_credentials() {
        let input = CONFIG.replace("[host]", "password = \"never-load-this\"\n\n[host]");
        assert!(matches!(
            HostConfig::parse(&input).unwrap_err(),
            ConfigError::InvalidSchema(_)
        ));
    }

    #[test]
    fn rejects_a_second_ups_in_v1() {
        let input = CONFIG.to_owned()
            + r#"

[ups.other]
profile = "dummy"
"#;
        assert!(matches!(
            HostConfig::parse(&input).unwrap_err(),
            ConfigError::MultipleUps(_)
        ));
    }

    #[test]
    fn explicit_and_environment_config_paths_take_precedence() {
        let host = FakeHost::default().with_env("NUT_CONFIG", "/tmp/nut.toml");
        assert_eq!(
            config_path(&host, None).unwrap(),
            (PathBuf::from("/tmp/nut.toml"), ConfigSource::Env)
        );
        assert_eq!(
            config_path(&host, Some(Path::new("/etc/nut-cli/host.toml"))).unwrap(),
            (PathBuf::from("/etc/nut-cli/host.toml"), ConfigSource::Flag)
        );
        let host = FakeHost::default();
        assert_eq!(
            config_path(&host, None).unwrap(),
            (
                PathBuf::from("/etc/nut-cli/nut.toml"),
                ConfigSource::Default
            )
        );
    }

    #[test]
    fn loads_and_resolves_athena_values_from_the_profile() {
        let path = PathBuf::from("/hosts/test/nut.toml");
        let host = FakeHost::default().with_file(&path, CONFIG.as_bytes().to_vec());
        let (_, _, config) = load_config(&host, Some(&path)).unwrap();
        let resolved = resolve(path, &config, &load_catalog(&host).unwrap()).unwrap();
        assert_eq!(resolved.ups_name, "cp1500");
        assert_eq!(resolved.offdelay.value.as_secs(), 60);
        assert_eq!(resolved.productid.value, "0601");
        assert_eq!(resolved.productid.source, ValueSource::Host);
        assert_eq!(resolved.driver.value, "usbhid-ups");
    }

    #[test]
    fn scaffold_maps_live_usb_and_dmi() {
        let host = FakeHost::default()
            .with_file("/etc/hostname", "athena")
            .with_file("/sys/class/dmi/id/board_vendor", "ASUSTeK COMPUTER INC.")
            .with_file("/sys/class/dmi/id/board_name", "PRIME TRX40-PRO")
            .with_file("/sys/bus/usb/devices/1-4/idVendor", "0764")
            .with_file("/sys/bus/usb/devices/1-4/idProduct", "0601");
        let catalog = load_catalog(&host).unwrap();
        let scaffold = scaffold_from_discovery(&host, &catalog).unwrap();
        assert!(scaffold.contains("name = \"athena\""));
        assert!(scaffold.contains("profile = \"cyberpower-cp1500pfclcd\""));
        assert!(scaffold.contains("productid = \"0601\""));
        assert!(scaffold.contains("board_profile = \"asus-prime-trx40-pro\""));
        assert!(scaffold.contains("example.invalid"));
    }
}

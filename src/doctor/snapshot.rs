use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::{ConfigError, HostConfig, ResolvedConfig, load_config, resolve};
use crate::host::{FileMetadata, Host};
use crate::profiles::{ProfileCatalog, UsbId, discover_usb_ids, load_catalog};
use crate::upsd::{UpsdClient, list_ups};

#[derive(Clone, Debug)]
pub struct UsbMatch {
    pub sysfs: PathBuf,
    pub id: UsbId,
    pub node: Option<PathBuf>,
    pub gid: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct UnitState {
    pub active: Option<String>,
    pub nrestarts: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub hostname: String,
    pub now: SystemTime,
    pub euid: u32,
    pub config_path: Option<PathBuf>,
    pub config: Option<HostConfig>,
    pub config_error: Option<String>,
    pub resolved: Option<ResolvedConfig>,
    pub catalog: ProfileCatalog,
    pub ups_name: String,
    pub vars: BTreeMap<String, String>,
    pub vars_error: Option<String>,
    pub cmds: Vec<String>,
    pub cmds_error: Option<String>,
    pub clients: Vec<String>,
    pub clients_error: Option<String>,
    pub nut_version: Option<String>,
    pub greeting: Option<String>,
    pub observe: Option<Duration>,
    pub binary: Option<PathBuf>,
    pub usb: Vec<UsbMatch>,
    pub usb_error: Option<String>,
    pub dummy_ups: bool,
}

pub struct GatherOptions<'a> {
    pub config_path: Option<&'a Path>,
    pub observe: Option<Duration>,
    pub ups: Option<&'a str>,
    pub address: &'a str,
    pub port: u16,
}

impl Snapshot {
    pub fn gather<H: Host>(host: &H, options: GatherOptions<'_>) -> Self {
        let hostname = host.hostname().unwrap_or_else(|_| "unknown".to_owned());
        let catalog = load_catalog(host)
            .or_else(|_| crate::profiles::embedded_catalog())
            .expect("embedded UPS and board catalog");
        let (loaded_path, config, config_error) = match load_config(host, options.config_path) {
            Ok((path, _, config)) => (Some(path), Some(config), None),
            Err(ConfigError::NotFound(path)) | Err(ConfigError::ReadFailure(path)) => {
                (Some(path), None, Some("host config is missing".to_owned()))
            }
            Err(error) => (None, None, Some(error.to_string())),
        };
        let resolved = config.as_ref().and_then(|config| {
            resolve(loaded_path.clone().unwrap_or_default(), config, &catalog).ok()
        });
        let dummy_ups = resolved
            .as_ref()
            .is_some_and(|resolved| resolved.driver.value == "dummy-ups");
        let address = options.address;
        let port = options.port;
        let ups_name = options
            .ups
            .map(str::to_owned)
            .or_else(|| resolved.as_ref().map(|resolved| resolved.ups_name.clone()))
            .or_else(|| {
                list_ups(address, port, Duration::from_millis(200))
                    .ok()
                    .and_then(|rows| rows.into_iter().next().map(|(name, _)| name))
            })
            .unwrap_or_else(|| "cp1500".to_owned());

        let mut vars = BTreeMap::new();
        let mut vars_error = None;
        let mut cmds = Vec::new();
        let mut cmds_error = None;
        let mut clients = Vec::new();
        let mut clients_error = None;
        let mut nut_version = None;
        let mut greeting = None;
        match UpsdClient::connect(address, port, &ups_name) {
            Ok(mut client) => {
                match client.list_vars() {
                    Ok(values) => vars = values,
                    Err(error) => vars_error = Some(error.to_string()),
                }
                let ver = client.version().ok();
                greeting = client
                    .greeting()
                    .map(ToOwned::to_owned)
                    .or_else(|| ver.clone());
                nut_version = greeting
                    .as_deref()
                    .and_then(parse_nut_version_text)
                    .or_else(|| ver.as_deref().and_then(parse_nut_version_text));
                match client.list_cmds() {
                    Ok(values) => cmds = values,
                    Err(error) => cmds_error = Some(error.to_string()),
                }
                match client.list_clients() {
                    Ok(values) => clients = values,
                    Err(error) => clients_error = Some(error.to_string()),
                }
            }
            Err(error) => {
                let message = error.to_string();
                vars_error = Some(message.clone());
                cmds_error = Some(message.clone());
                clients_error = Some(message);
            }
        }

        let (usb, usb_error) = match discover_usb_ids(host) {
            Ok(found) => {
                let wanted = wanted_usb_ids(resolved.as_ref(), &catalog);
                let matches = found
                    .into_iter()
                    .filter(|(_, id)| usb_wanted(&wanted, *id))
                    .map(|(sysfs, id)| {
                        let node = usb_device_node(host, &sysfs);
                        let gid = node.as_ref().and_then(|path| {
                            host.metadata(path).ok().flatten().map(|meta| meta.gid)
                        });
                        UsbMatch {
                            sysfs,
                            id,
                            node,
                            gid,
                        }
                    })
                    .collect();
                (matches, None)
            }
            Err(error) => (Vec::new(), Some(error.to_string())),
        };

        Self {
            hostname,
            now: host.now(),
            euid: host.euid(),
            config_path: loaded_path,
            config,
            config_error,
            resolved,
            catalog,
            ups_name,
            vars,
            vars_error,
            cmds,
            cmds_error,
            clients,
            clients_error,
            nut_version,
            greeting,
            observe: options.observe,
            binary: host.current_exe().ok(),
            usb,
            usb_error,
            dummy_ups,
        }
    }

    pub fn is_root(&self) -> bool {
        self.euid == 0
    }

    pub fn status_flags(&self) -> Vec<String> {
        self.vars
            .get("ups.status")
            .map(|value| {
                value
                    .split_ascii_whitespace()
                    .map(|flag| flag.to_ascii_uppercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn float_var(&self, name: &str) -> Option<f64> {
        self.vars
            .get(name)?
            .parse()
            .ok()
            .filter(|value: &f64| value.is_finite())
    }

    pub fn group_gid<H: Host>(&self, host: &H, name: &str) -> Option<u32> {
        parse_group_file(host, |group, gid, _| (group == name).then_some(gid))
    }
}

fn wanted_usb_ids(resolved: Option<&ResolvedConfig>, catalog: &ProfileCatalog) -> Vec<UsbId> {
    if let Some(resolved) = resolved {
        if let (Ok(vendor), Ok(product)) = (
            u16::from_str_radix(&resolved.vendorid.value, 16),
            u16::from_str_radix(&resolved.productid.value, 16),
        ) {
            return vec![UsbId { vendor, product }];
        }
        if !resolved.profile.usb_ids.is_empty() {
            return resolved.profile.usb_ids.clone();
        }
    }
    catalog
        .ups_profiles()
        .flat_map(|profile| profile.usb_ids.iter().copied())
        .collect()
}

fn usb_wanted(wanted: &[UsbId], id: UsbId) -> bool {
    !wanted.is_empty() && wanted.contains(&id)
}

fn usb_device_node<H: Host>(host: &H, sysfs: &Path) -> Option<PathBuf> {
    let bus = read_sys(host, &sysfs.join("busnum"))?;
    let dev = read_sys(host, &sysfs.join("devnum"))?;
    let bus = bus.parse::<u32>().ok()?;
    let dev = dev.parse::<u32>().ok()?;
    Some(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}")))
}

fn read_sys<H: Host>(host: &H, path: &Path) -> Option<String> {
    let bytes = host.read_file(path).ok().flatten()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn parse_group_file<H: Host, T>(
    host: &H,
    mut pick: impl FnMut(&str, u32, &str) -> Option<T>,
) -> Option<T> {
    let bytes = host.read_file(Path::new("/etc/group")).ok().flatten()?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _x = fields.next()?;
        let gid = fields.next()?.parse::<u32>().ok()?;
        let members = fields.next().unwrap_or("");
        if let Some(value) = pick(name, gid, members) {
            return Some(value);
        }
    }
    None
}

pub fn parse_nut_version_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| {
            let mut parts = token.split('.');
            parts
                .next()
                .is_some_and(|part| part.chars().all(|ch| ch.is_ascii_digit()))
                && parts.next().is_some()
        })
        .map(|token| {
            token
                .split(|ch: char| !ch.is_ascii_digit() && ch != '.')
                .next()
                .unwrap_or(token)
                .to_owned()
        })
}

pub fn parse_version_triple(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .and_then(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0);
    Some((major, minor, patch))
}

pub fn unit_state<H: Host>(host: &H, unit: &str) -> UnitState {
    let show = host.run_program(
        "systemctl".as_ref(),
        &[
            OsString::from("show"),
            OsString::from(unit),
            OsString::from("-p"),
            OsString::from("ActiveState"),
            OsString::from("-p"),
            OsString::from("NRestarts"),
        ],
    );
    let mut active = None;
    let mut nrestarts = None;
    if let Ok(output) = show {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "ActiveState" => active = Some(value.to_owned()),
                    "NRestarts" => nrestarts = Some(value.to_owned()),
                    _ => {}
                }
            }
        }
    }
    let enabled = host
        .run_program(
            "systemctl".as_ref(),
            &[OsString::from("is-enabled"), OsString::from(unit)],
        )
        .ok()
        .map(|output| {
            output.status == Some(0) && String::from_utf8_lossy(&output.stdout).trim() == "enabled"
        });
    UnitState {
        active,
        nrestarts,
        enabled,
    }
}

pub fn loopback_listeners<H: Host>(host: &H) -> Result<Vec<IpAddr>, String> {
    let mut listeners = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        match host.read_file(Path::new(path)) {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                listeners.extend(parse_proc_net_listeners(&text));
            }
            Ok(None) => {}
            Err(error) if error.is_permission_denied() => {
                return Err(format!("{path} is unreadable"));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(listeners)
}

fn parse_proc_net_listeners(text: &str) -> Vec<IpAddr> {
    let mut listeners = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        // st 0A is TCP_LISTEN
        if fields.get(3) != Some(&"0A") {
            continue;
        }
        let Some((addr, port)) = fields[1].split_once(':') else {
            continue;
        };
        if u16::from_str_radix(port, 16) != Ok(3493) {
            continue;
        }
        if let Some(ip) = parse_hex_ip(addr) {
            listeners.push(ip);
        }
    }
    listeners
}

fn parse_hex_ip(hex: &str) -> Option<IpAddr> {
    if hex.len() == 8 {
        let raw = u32::from_str_radix(hex, 16).ok()?;
        let bytes = raw.to_be_bytes();
        return Some(IpAddr::V4(Ipv4Addr::new(
            bytes[3], bytes[2], bytes[1], bytes[0],
        )));
    }
    if hex.len() == 32 {
        let mut bytes = [0u8; 16];
        for (index, chunk) in hex.as_bytes().chunks(8).enumerate() {
            let word = std::str::from_utf8(chunk).ok()?;
            let raw = u32::from_str_radix(word, 16).ok()?.swap_bytes();
            bytes[index * 4..index * 4 + 4].copy_from_slice(&raw.to_be_bytes());
        }
        return Some(IpAddr::V6(Ipv6Addr::from(bytes)));
    }
    None
}

pub fn file_meta<H: Host>(host: &H, path: &Path) -> Result<Option<FileMetadata>, String> {
    host.metadata(path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_net_tcp_parses_loopback_listen_on_3493() {
        let ipv4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0DA5 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 0 0 0 10 -1\n";
        assert_eq!(
            parse_proc_net_listeners(ipv4),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
        let ipv6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000000000000000000001000000:0DA5 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 0 0 0 10 -1\n";
        assert_eq!(
            parse_proc_net_listeners(ipv6),
            vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]
        );
    }

    #[test]
    fn nut_version_parses_greeting_and_debian_suffix() {
        assert_eq!(
            parse_nut_version_text("Network UPS Tools upsd 2.8.1 - fake"),
            Some("2.8.1".into())
        );
        assert_eq!(parse_version_triple("2.8.3-1"), Some((2, 8, 3)));
        assert!(parse_version_triple("2.8.1") < parse_version_triple("2.8.3"));
    }
}

use std::collections::BTreeMap;
use std::time::Duration;

use crate::upsd::{UpsdClient, list_ups};

use super::HostError;

/// Read-only UPS evidence collected at the observation boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpsObservation {
    pub vars: BTreeMap<String, String>,
    pub vars_error: Option<String>,
    pub cmds: Vec<String>,
    pub cmds_error: Option<String>,
    pub clients: Vec<String>,
    pub clients_error: Option<String>,
    pub nut_version: Option<String>,
    pub greeting: Option<String>,
}

impl UpsObservation {
    pub fn unavailable(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            vars_error: Some(message.clone()),
            cmds_error: Some(message.clone()),
            clients_error: Some(message),
            ..Self::default()
        }
    }
}

pub(super) fn collect_native(address: &str, port: u16, ups: &str) -> UpsObservation {
    match UpsdClient::connect(address, port, ups) {
        Ok(mut client) => {
            let mut observation = UpsObservation::default();
            match client.list_vars() {
                Ok(values) => observation.vars = values,
                Err(error) => observation.vars_error = Some(error.to_string()),
            }
            let version = client.version().ok();
            observation.greeting = client
                .greeting()
                .map(ToOwned::to_owned)
                .or_else(|| version.clone());
            observation.nut_version = observation
                .greeting
                .as_deref()
                .and_then(parse_nut_version)
                .or_else(|| version.as_deref().and_then(parse_nut_version));
            match client.list_cmds() {
                Ok(values) => observation.cmds = values,
                Err(error) => observation.cmds_error = Some(error.to_string()),
            }
            match client.list_clients() {
                Ok(values) => observation.clients = values,
                Err(error) => observation.clients_error = Some(error.to_string()),
            }
            observation
        }
        Err(error) => UpsObservation::unavailable(error.to_string()),
    }
}

pub(super) fn list_native(address: &str, port: u16) -> Result<Vec<(String, String)>, HostError> {
    list_ups(address, port, Duration::from_millis(200))
        .map_err(|error| HostError::message(error.to_string()))
}

fn parse_nut_version(text: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use crate::host::{Host, SystemHost};

    #[test]
    fn native_observation_on_closed_port_is_unavailable() {
        let observation = SystemHost.observe_ups("127.0.0.1", 1, "cp1500");
        assert!(
            observation
                .vars_error
                .as_deref()
                .is_some_and(|error| error.contains("cannot connect to upsd")),
            "{observation:?}"
        );
        assert!(observation.vars.is_empty());
    }
}

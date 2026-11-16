//! Read-only extension seam: return Some (including an empty list) to own an
//! argument slot, or None to let clap supply its enum/path/command candidates.
use std::ffi::OsString;
use std::time::Duration;

use crate::host::Host;

pub struct Context {
    pub command: Vec<String>,
    pub argument: String,
    pub words: Vec<OsString>,
}

pub struct Candidate {
    pub value: String,
    pub description: String,
    pub group: String,
    pub nospace: bool,
}

pub trait Provider {
    fn values(&self, context: &Context) -> Option<Vec<Candidate>>;
}

pub(super) struct LiveProviders<'a, H>(pub &'a H);

impl<H: Host> Provider for LiveProviders<'_, H> {
    fn values(&self, context: &Context) -> Option<Vec<Candidate>> {
        let pairs = match (
            context
                .command
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice(),
            context.argument.as_str(),
        ) {
            (_, "ups") => {
                let address =
                    option(&context.words, "--upsd-address").unwrap_or_else(|| "127.0.0.1".into());
                let port = option(&context.words, "--upsd-port")
                    .map_or(Some(3493), |port| port.parse().ok());
                port.and_then(|port| {
                    crate::upsd::list_ups(&address, port, Duration::from_millis(150)).ok()
                })
                .unwrap_or_default()
            }
            (["bios", "profile", "show"], "id") => crate::firmware::load_board_catalog(self.0)
                .ok()
                .map(|catalog| {
                    catalog
                        .board_profiles()
                        .map(|profile| {
                            (
                                profile.id.as_str().to_owned(),
                                profile.display_name().to_owned(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            // Later phases can extend this match with doctor IDs and drill records.
            (["rollback"], "stamp") => self
                .0
                .list_dir(std::path::Path::new("/var/backups/nut-cli"))
                .unwrap_or_default()
                .into_iter()
                .map(|stamp| (stamp, "Configuration backup".into()))
                .collect(),
            _ => return None,
        };
        Some(
            pairs
                .into_iter()
                .map(|(value, description)| Candidate {
                    value,
                    description,
                    group: context.argument.clone(),
                    nospace: false,
                })
                .collect(),
        )
    }
}

fn option(words: &[OsString], name: &str) -> Option<String> {
    let mut found = None;
    let mut words = words.iter().skip(1).peekable();
    while let Some(word) = words.next() {
        if word == "--" {
            break;
        }
        if word == name {
            found = words
                .next()
                .map(|value| value.to_string_lossy().into_owned());
        } else if let Some(value) = word.to_string_lossy().strip_prefix(&format!("{name}=")) {
            found = Some(value.to_owned());
        }
    }
    found
}

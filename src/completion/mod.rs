//! Protocol 2, menu-backed completion, and per-user adapter lifecycle.
pub mod engine;
mod lifecycle;
pub mod menu;
pub mod protocol;
pub mod providers;

use std::ffi::OsString;
use std::io::Write;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::host::Host;

pub use lifecycle::run;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// Z shell.
    Zsh,
    /// GNU Bash.
    Bash,
}

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
        }
    }

    pub fn adapter(self) -> &'static str {
        match self {
            Self::Zsh => include_str!("adapters/_nut.zsh"),
            Self::Bash => include_str!("adapters/nut.bash"),
        }
    }

    fn filename(self) -> &'static str {
        match self {
            Self::Zsh => "_nut",
            Self::Bash => "nut",
        }
    }
}

fn ascii_mode<H: Host>(host: &H) -> bool {
    let term = host.env("TERM");
    let locale = host
        .env("LC_ALL")
        .or_else(|| host.env("LC_CTYPE"))
        .or_else(|| host.env("LANG"));
    let glyphs = crate::ui::glyphs::Glyphs::new(false, term.as_deref(), locale.as_deref());
    glyphs.ellipsis == "..."
}

pub fn complete<H: Host, O: Write, E: Write>(
    argv: &[OsString],
    host: &H,
    stdout: &mut O,
    stderr: &mut E,
) -> i32 {
    let request = match protocol::parse_request(argv) {
        Ok(request) => request,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return 2;
        }
    };
    let lines = protocol::skew_message(request.protocol).map_or_else(
        || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let ascii = ascii_mode(host);
                let (keep, menu) = engine::complete(&request, &providers::LiveProviders(host));
                protocol::serialize(
                    &menu,
                    keep,
                    request.shell,
                    request.protocol,
                    request.columns,
                    ascii,
                )
            }))
            .unwrap_or_default()
        },
        |message| vec![message],
    );
    if !lines.is_empty() {
        let _ = writeln!(stdout, "{}", lines.join("\n"));
    }
    0
}

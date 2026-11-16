use crate::cli::ColorChoice;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    Ok,
    Warn,
    Fail,
    Info,
    Secondary,
}

pub struct Theme;

impl Theme {
    pub const OK: &str = "\x1b[32m";
    pub const WARN: &str = "\x1b[33m";
    pub const FAIL: &str = "\x1b[31m";
    pub const INFO: &str = "\x1b[36m";
    pub const SECONDARY: &str = "\x1b[2m";
    pub const ACCENT: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";

    pub fn paint(tone: Tone, text: &str, enabled: bool) -> String {
        if !enabled {
            return text.to_owned();
        }
        let color = match tone {
            Tone::Ok => Self::OK,
            Tone::Warn => Self::WARN,
            Tone::Fail => Self::FAIL,
            Tone::Info => Self::INFO,
            Tone::Secondary => Self::SECONDARY,
        };
        format!("{color}{text}{}", Self::RESET)
    }

    pub fn accent(text: &str, enabled: bool) -> String {
        if enabled {
            format!("{}{text}{}", Self::ACCENT, Self::RESET)
        } else {
            text.to_owned()
        }
    }
}

/// Resolve the CLI choice and the standard NO_COLOR / CLICOLOR_FORCE environment policy.
///
/// An explicit command-line choice has precedence over environment defaults. Auto never adds
/// ANSI escapes to a pipe unless CLICOLOR_FORCE is set.
pub fn color_enabled(
    choice: ColorChoice,
    output_is_terminal: bool,
    no_color: Option<&std::ffi::OsStr>,
    force_color: Option<&std::ffi::OsStr>,
) -> bool {
    match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => {
            if force_color.is_some_and(|value| !value.is_empty() && value != "0") {
                return true;
            }
            if no_color.is_some_and(|value| !value.is_empty()) {
                return false;
            }
            output_is_terminal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn auto_color_respects_terminal_and_environment() {
        assert!(!color_enabled(ColorChoice::Auto, false, None, None));
        assert!(color_enabled(ColorChoice::Auto, true, None, None));
        assert!(!color_enabled(
            ColorChoice::Auto,
            true,
            Some(OsStr::new("1")),
            None
        ));
        assert!(color_enabled(
            ColorChoice::Auto,
            false,
            None,
            Some(OsStr::new("1"))
        ));
        assert!(!color_enabled(
            ColorChoice::Auto,
            false,
            None,
            Some(OsStr::new("0"))
        ));
    }

    #[test]
    fn explicit_color_choice_overrides_environment_defaults() {
        assert!(!color_enabled(
            ColorChoice::Never,
            true,
            None,
            Some(OsStr::new("1"))
        ));
        assert!(color_enabled(
            ColorChoice::Always,
            false,
            Some(OsStr::new("1")),
            None
        ));
    }

    #[test]
    fn theme_is_the_single_source_for_semantic_ansi_colors() {
        assert_eq!(
            Theme::paint(Tone::Ok, "ready", true),
            "\x1b[32mready\x1b[0m"
        );
        assert_eq!(Theme::paint(Tone::Warn, "wait", false), "wait");
    }
}

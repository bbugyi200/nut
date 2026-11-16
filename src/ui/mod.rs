pub mod glyphs;
pub mod layout;
pub mod theme;

use crate::cli::ColorChoice;
use crate::host::Host;

#[derive(Clone, Copy, Debug)]
pub struct UiOptions {
    pub color: ColorChoice,
    pub ascii: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiContext {
    pub color_enabled: bool,
    pub glyphs: glyphs::Glyphs,
    pub width: usize,
}

impl UiContext {
    pub fn new<H: Host>(options: UiOptions, host: &H, stdout_is_terminal: bool) -> Self {
        let color_enabled = theme::color_enabled(
            options.color,
            stdout_is_terminal,
            host.env("NO_COLOR").as_deref(),
            host.env("CLICOLOR_FORCE").as_deref(),
        );
        let term = host.env("TERM");
        let locale = host
            .env("LC_ALL")
            .or_else(|| host.env("LC_CTYPE"))
            .or_else(|| host.env("LANG"));
        let glyphs = glyphs::Glyphs::new(options.ascii, term.as_deref(), locale.as_deref());
        let columns = host.env("COLUMNS");
        let width = layout::terminal_width(
            columns.as_deref().and_then(|value| value.to_str()),
            host.terminal_columns(),
        );
        Self {
            color_enabled,
            glyphs,
            width,
        }
    }
}

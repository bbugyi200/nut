#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Glyphs {
    pub ok: &'static str,
    pub warn: &'static str,
    pub fail: &'static str,
    pub pending: &'static str,
    pub bullet: &'static str,
    pub power: &'static str,
    pub arrow: &'static str,
    pub bar_full: &'static str,
    pub bar_empty: &'static str,
}

impl Glyphs {
    pub fn new(
        ascii: bool,
        term: Option<&std::ffi::OsStr>,
        locale: Option<&std::ffi::OsStr>,
    ) -> Self {
        let term_dumb = term.is_some_and(|value| value == "dumb");
        let utf8_locale = locale
            .and_then(|value| value.to_str())
            .is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("utf-8") || value.contains("utf8")
            });
        if ascii || term_dumb || !utf8_locale {
            Self {
                ok: "ok",
                warn: "!!",
                fail: "xx",
                pending: "--",
                bullet: ">",
                power: "*",
                arrow: "->",
                bar_full: "#",
                bar_empty: ".",
            }
        } else {
            Self {
                ok: "✓",
                warn: "⚠",
                fail: "✗",
                pending: "○",
                bullet: "▸",
                power: "⚡",
                arrow: "→",
                bar_full: "█",
                bar_empty: " ",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn unicode_glyphs_need_utf8_locale_and_non_dumb_terminal() {
        let unicode = Glyphs::new(
            false,
            Some(OsStr::new("xterm-256color")),
            Some(OsStr::new("C.UTF-8")),
        );
        assert_eq!(unicode.ok, "✓");
        let dumb = Glyphs::new(false, Some(OsStr::new("dumb")), Some(OsStr::new("C.UTF-8")));
        assert_eq!(dumb.ok, "ok");
        let c_locale = Glyphs::new(false, Some(OsStr::new("xterm")), Some(OsStr::new("C")));
        assert_eq!(c_locale.arrow, "->");
        let forced = Glyphs::new(true, Some(OsStr::new("xterm")), Some(OsStr::new("C.UTF-8")));
        assert_eq!(forced.power, "*");
    }
}

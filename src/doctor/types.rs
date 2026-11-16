use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Tool,
    Hardware,
    Services,
    Protection,
    Root,
    Boot,
    Proof,
}

impl Section {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Hardware => "hw",
            Self::Services => "svc",
            Self::Protection => "protect",
            Self::Root => "root",
            Self::Boot => "boot",
            Self::Proof => "proof",
        }
    }

    pub fn heading(self) -> &'static str {
        match self {
            Self::Tool => "This tool",
            Self::Hardware => "Hardware",
            Self::Services => "Services",
            Self::Protection => "Protection",
            Self::Root => "Root-only",
            Self::Boot => "Boot & BIOS",
            Self::Proof => "Proof",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Unknown,
    Skipped,
}

impl Status {
    pub fn glyph(self, glyphs: &crate::ui::glyphs::Glyphs) -> &'static str {
        match self {
            Self::Pass => glyphs.ok,
            Self::Warn => glyphs.warn,
            Self::Fail => glyphs.fail,
            Self::Unknown | Self::Skipped => glyphs.pending,
        }
    }

    pub fn tone(self) -> crate::ui::theme::Tone {
        match self {
            Self::Pass => crate::ui::theme::Tone::Ok,
            Self::Warn => crate::ui::theme::Tone::Warn,
            Self::Fail => crate::ui::theme::Tone::Fail,
            Self::Unknown | Self::Skipped => crate::ui::theme::Tone::Secondary,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Check {
    pub id: String,
    pub section: Section,
    pub status: Status,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    pub needs_root: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub section: Section,
    pub title: &'static str,
    pub docs: &'static str,
    pub needs_root: bool,
}

pub fn pass(entry: &CatalogEntry, evidence: impl Into<String>) -> Check {
    Check {
        id: entry.id.to_owned(),
        section: entry.section,
        status: Status::Pass,
        evidence: evidence.into(),
        fix: None,
        needs_root: entry.needs_root,
    }
}

pub fn warn(entry: &CatalogEntry, evidence: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        id: entry.id.to_owned(),
        section: entry.section,
        status: Status::Warn,
        evidence: evidence.into(),
        fix: Some(fix.into()),
        needs_root: entry.needs_root,
    }
}

pub fn fail(entry: &CatalogEntry, evidence: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        id: entry.id.to_owned(),
        section: entry.section,
        status: Status::Fail,
        evidence: evidence.into(),
        fix: Some(fix.into()),
        needs_root: entry.needs_root,
    }
}

pub fn unknown(entry: &CatalogEntry, evidence: impl Into<String>) -> Check {
    Check {
        id: entry.id.to_owned(),
        section: entry.section,
        status: Status::Unknown,
        evidence: evidence.into(),
        fix: None,
        needs_root: entry.needs_root,
    }
}

pub fn skipped(entry: &CatalogEntry, evidence: impl Into<String>, fix: Option<String>) -> Check {
    Check {
        id: entry.id.to_owned(),
        section: entry.section,
        status: Status::Skipped,
        evidence: evidence.into(),
        fix,
        needs_root: entry.needs_root,
    }
}

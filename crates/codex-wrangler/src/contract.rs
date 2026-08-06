//! Tester-independent vocabulary shared across the Wrangler GUI boundary.

#[cfg(feature = "egui-test")]
use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(feature = "egui-test")]
pub const UI_FINGERPRINT: &str = "codex-wrangler.ui/6";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    Codex,
    ClaudeCode,
    PrimeAgent,
}

impl Harness {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::PrimeAgent => "prime-agent",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Work {
    Input,
    Goal,
    Turn,
    Done,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CardObservation {
    pub harness: Harness,
    pub name: Option<String>,
    pub thread: String,
    pub work: Work,
    pub workspace: Option<u32>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Observation {
    pub fingerprint: String,
    pub summoning: bool,
    pub hovered: Option<CardKey>,
    pub loading: bool,
    pub cards: Vec<CardObservation>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CardKey {
    pub harness: Harness,
    pub thread: String,
}

#[cfg(feature = "egui-test")]
pub struct CardTarget<'a>(pub Harness, pub &'a str);

#[cfg(feature = "egui-test")]
pub struct LogoTarget<'a>(pub Harness, pub &'a str);

#[cfg(feature = "egui-test")]
pub struct WorkspaceTarget<'a>(pub Harness, pub &'a str);

#[cfg(feature = "egui-test")]
impl fmt::Display for CardTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/activate", self.0.slug(), self.1)
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for LogoTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/logo", self.0.slug(), self.1)
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for WorkspaceTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/workspace", self.0.slug(), self.1)
    }
}

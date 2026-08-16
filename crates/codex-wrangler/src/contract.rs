//! Tester-independent vocabulary shared across the Wrangler GUI boundary.

#[cfg(feature = "egui-test")]
use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(feature = "egui-test")]
pub const UI_FINGERPRINT: &str = "codex-wrangler.ui/16";

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
    Error,
    Input,
    Goal,
    Turn,
    Sleep,
    Done,
    Closed,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Flight {
    Grounded,
    Striking,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tab {
    Live,
    Historical,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteGuard {
    Armed,
    Bypassed,
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
    pub jiggling: bool,
    pub flight: Flight,
    pub search: SearchObservation,
    pub guide: GuideVisibility,
    pub tab: Tab,
    pub delete_guard: DeleteGuard,
    pub delete_prompt: Option<String>,
    pub visible: Vec<CardKey>,
    pub cards: Vec<CardObservation>,
    pub history: Vec<HistoryObservation>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryObservation {
    pub thread: String,
    pub name: Option<String>,
    pub turns: Option<u64>,
    pub bytes: u64,
    pub archived: bool,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchObservation {
    pub query: String,
    pub valid: bool,
    pub focused: bool,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuideVisibility {
    Closed,
    Open,
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
pub struct WorkspaceTarget<'a>(pub Harness, pub &'a str);

#[cfg(feature = "egui-test")]
pub struct TabTarget(pub Tab);

#[cfg(feature = "egui-test")]
pub struct HistoryTarget<'a>(pub &'a str, pub &'static str);

#[cfg(feature = "egui-test")]
impl fmt::Display for CardTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/activate", self.0.slug(), self.1)
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for WorkspaceTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/workspace", self.0.slug(), self.1)
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for TabTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "tab/{}",
            match self.0 {
                Tab::Live => "live",
                Tab::Historical => "historical",
            }
        )
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for HistoryTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "history/{}/{}", self.0, self.1)
    }
}

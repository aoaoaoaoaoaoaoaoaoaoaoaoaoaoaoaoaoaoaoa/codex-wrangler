//! Tester-independent vocabulary shared across the Wrangler GUI boundary.

#[cfg(feature = "egui-test")]
use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(feature = "egui-test")]
pub const UI_FINGERPRINT: &str = "codex-wrangler.ui/4";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Work {
    Goal,
    Turn,
    Done,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CardObservation {
    pub name: Option<String>,
    pub thread: String,
    pub work: Work,
    pub workspace: Option<u32>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Observation {
    pub fingerprint: String,
    pub hovered: Option<String>,
    pub loading: bool,
    pub cards: Vec<CardObservation>,
}

#[cfg(feature = "egui-test")]
pub struct CardTarget<'a>(pub &'a str);

#[cfg(feature = "egui-test")]
impl fmt::Display for CardTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "codex/{}/activate", self.0)
    }
}

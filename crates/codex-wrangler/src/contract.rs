//! Tester-independent vocabulary shared across the Wrangler GUI boundary.

#[cfg(feature = "egui-test")]
use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(feature = "egui-test")]
pub const UI_FINGERPRINT: &str = "codex-wrangler.ui/27";

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryColumn {
    SessionId,
    Name,
    LastTurn,
    Turns,
    Size,
    State,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryOperation {
    Archive,
    Unarchive,
    Delete,
    Rename,
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
pub enum ForkField {
    Quiescent,
    Armed,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinField {
    Quiescent,
    Armed,
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClosePreference {
    Exit,
    Minimize,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CardObservation {
    pub harness: Harness,
    pub name: Option<String>,
    pub thread: String,
    pub work: Work,
    pub workspace: Option<u32>,
    pub pinned: bool,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Observation {
    pub fingerprint: String,
    pub summoning: bool,
    pub hovered: Option<CardKey>,
    pub loading: bool,
    pub jiggling: bool,
    pub fork_field: ForkField,
    pub pin_field: PinField,
    pub flight: Flight,
    pub search: SearchObservation,
    pub guide: GuideVisibility,
    pub settings: SettingsObservation,
    pub tab: Tab,
    pub delete_guard: DeleteGuard,
    pub close_preference: ClosePreference,
    pub delete_prompt: Option<String>,
    pub visible: Vec<CardKey>,
    pub cards: Vec<CardObservation>,
    pub history: Vec<HistoryObservation>,
    pub history_rename: Option<HistoryRenameObservation>,
    pub history_transcript: Option<HistoryTranscriptObservation>,
    pub history_order: Vec<String>,
    pub history_sorts: Vec<HistorySortObservation>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryObservation {
    pub thread: String,
    pub name: Option<String>,
    pub turns: Option<u64>,
    pub bytes: u64,
    pub archived: bool,
    pub pending: Option<HistoryOperation>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryTranscriptObservation {
    pub thread: String,
    pub cursor: Option<usize>,
    pub total: usize,
    pub user: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryRenameObservation {
    pub thread: String,
    pub draft: String,
    pub focused: bool,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchObservation {
    pub query: String,
    pub valid: bool,
    pub focused: bool,
    pub editing: bool,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SettingsObservation {
    pub open: bool,
    pub fault: bool,
    pub settled: bool,
}

#[cfg(feature = "egui-test")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistorySortObservation {
    pub column: HistoryColumn,
    pub direction: SortDirection,
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
pub enum SearchTarget {
    Editor,
    Filter,
}

#[cfg(feature = "egui-test")]
pub struct HistorySortTarget(pub HistoryColumn);

#[cfg(feature = "egui-test")]
pub struct PreferenceTarget(pub &'static str);

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
impl fmt::Display for SearchTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Editor => "search/editor",
            Self::Filter => "search/filter",
        })
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for HistorySortTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "history/sort/{}",
            match self.0 {
                HistoryColumn::SessionId => "session-id",
                HistoryColumn::Name => "name",
                HistoryColumn::LastTurn => "last-turn",
                HistoryColumn::Turns => "turns",
                HistoryColumn::Size => "size",
                HistoryColumn::State => "state",
            }
        )
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for PreferenceTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "preferences/{}", self.0)
    }
}

#[cfg(feature = "egui-test")]
impl fmt::Display for HistoryTarget<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "history/{}/{}", self.0, self.1)
    }
}

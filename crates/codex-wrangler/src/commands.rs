use std::sync::OnceLock;

use eternalist_apps::{
    command_guide::{GuideGesture, GuideSection},
    commands::{CommandCanon, CommandScope, CommandSpec, Shortcut, ShortcutKey, ShortcutModifiers},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edict {
    Scry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Realm {
    Gallery,
}

const SCRY_KEYS: [Shortcut; 1] = [Shortcut::new(ShortcutModifiers::NONE, ShortcutKey::Slash)];
const TAB: [Shortcut; 1] = [Shortcut::new(ShortcutModifiers::NONE, ShortcutKey::Tab)];
const ESCAPE: [Shortcut; 1] = [Shortcut::new(ShortcutModifiers::NONE, ShortcutKey::Escape)];
const NAVIGATION_GESTURES: [GuideGesture; 1] = [GuideGesture::new(
    "Switch tab",
    "Cycles Live and Historical.",
    &TAB,
)];
const SCRY_GESTURES: [GuideGesture; 1] = [GuideGesture::new(
    "Clear search",
    "Clears the current tab's filter without hiding Wrangler.",
    &ESCAPE,
)];

pub const NAVIGATION_IDIOMS: GuideSection = GuideSection::new("NAVIGATION", &NAVIGATION_GESTURES);
pub const SCRY_IDIOMS: GuideSection = GuideSection::new("SEARCH", &SCRY_GESTURES);

const EDICTS: [CommandSpec<Edict, Realm>; 1] = [CommandSpec::new(
    Edict::Scry,
    "gallery.search",
    "Search current tab",
    CommandScope::Context(Realm::Gallery),
)
.with_detail(
    "Case-insensitive regexp. Live searches names or nameless paths; Historical searches names and session IDs.",
)
.with_default_shortcuts(&SCRY_KEYS)];

pub fn canon() -> &'static CommandCanon<Edict, Realm> {
    static CANON: OnceLock<CommandCanon<Edict, Realm>> = OnceLock::new();
    CANON.get_or_init(|| CommandCanon::new(&EDICTS))
}

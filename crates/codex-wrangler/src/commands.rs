use std::sync::OnceLock;

use eternalist_apps::{
    command_guide::{GuideGesture, GuideGroup},
    commands::{
        CommandCanon, CommandScope, CommandSpec, SETTINGS_SHORTCUTS, Shortcut, ShortcutKey,
        ShortcutModifiers,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edict {
    Search,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Gallery,
}

const SEARCH_KEYS: [Shortcut; 1] = [Shortcut::new(ShortcutModifiers::NONE, ShortcutKey::Slash)];
const TAB: [Shortcut; 1] = [Shortcut::new(ShortcutModifiers::NONE, ShortcutKey::Tab)];
const ESCAPE: [Shortcut; 1] = [Shortcut::new(ShortcutModifiers::NONE, ShortcutKey::Escape)];
const APPLICATION_GESTURES: [GuideGesture; 1] = [GuideGesture::new(
    "Open settings",
    "Opens Wrangler's complete configuration surface.",
    &SETTINGS_SHORTCUTS,
)];
const NAVIGATION_GESTURES: [GuideGesture; 1] = [GuideGesture::new(
    "Switch tab",
    "Cycles Live and Historical.",
    &TAB,
)];
const SEARCH_GESTURES: [GuideGesture; 1] = [GuideGesture::new(
    "Clear search",
    "Clears the current tab's filter without hiding Wrangler.",
    &ESCAPE,
)];
const TILE_GESTURES: [GuideGesture; 2] = [
    GuideGesture::new(
        "Ctrl+click Codex tile",
        "Forks the chat in a new Alacritty on its workspace.",
        &[],
    ),
    GuideGesture::new(
        "Alt+click tile",
        "Pins or unpins the session; pinned sessions form the head bucket.",
        &[],
    ),
];

pub const NAVIGATION_GUIDE_GROUP: GuideGroup = GuideGroup::new("NAVIGATION", &NAVIGATION_GESTURES);
pub const APPLICATION_GUIDE_GROUP: GuideGroup =
    GuideGroup::new("APPLICATION", &APPLICATION_GESTURES);
pub const SEARCH_GUIDE_GROUP: GuideGroup = GuideGroup::new("SEARCH", &SEARCH_GESTURES);
pub const TILE_GUIDE_GROUP: GuideGroup = GuideGroup::new("TILES", &TILE_GESTURES);

const EDICTS: [CommandSpec<Edict, Context>; 1] = [CommandSpec::new(
    Edict::Search,
    "gallery.search",
    "Search current tab",
    CommandScope::Context(Context::Gallery),
)
.with_detail(
    "Case-insensitive regexp. Live searches names or nameless paths; Historical searches names and session IDs.",
)
.with_default_shortcuts(&SEARCH_KEYS)];

pub fn canon() -> &'static CommandCanon<Edict, Context> {
    static CANON: OnceLock<CommandCanon<Edict, Context>> = OnceLock::new();
    CANON.get_or_init(|| CommandCanon::new(&EDICTS))
}

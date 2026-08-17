use std::cmp::Ordering;

use crate::contract::{Harness, Work};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    pub harness: Harness,
    pub thread: String,
    pub name: Option<String>,
    pub cwd: String,
    pub tile_preview: String,
    pub work: Work,
    pub window: Option<u32>,
    pub workspace: Option<u32>,
    pub updated_at_ms: i64,
    pub pinned: bool,
}

impl Card {
    pub fn assert_lawful(&self) {
        assert_eq!(
            self.work == Work::Closed,
            self.window.is_none(),
            "Closed must be exactly terminal absence"
        );
    }
}

pub fn snip(text: &str, limit: usize) -> String {
    let mut flat = String::with_capacity(text.len().min(limit + 1));
    for word in text.split_whitespace() {
        if !flat.is_empty() {
            flat.push(' ');
        }
        flat.push_str(word);
        if flat.chars().count() > limit {
            break;
        }
    }
    let mut chars = flat.chars();
    let head = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

impl Ord for Card {
    fn cmp(&self, other: &Self) -> Ordering {
        other.pinned.cmp(&self.pinned).then_with(|| {
            rank(self.work)
                .cmp(&rank(other.work))
                .then_with(|| other.updated_at_ms.cmp(&self.updated_at_ms))
                .then_with(|| self.harness.cmp(&other.harness))
                .then_with(|| self.thread.cmp(&other.thread))
        })
    }
}

impl PartialOrd for Card {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const fn rank(work: Work) -> u8 {
    match work {
        Work::Error => 0,
        Work::Input => 1,
        Work::Goal => 2,
        Work::Turn => 3,
        Work::Sleep | Work::Done => 4,
        Work::Closed => 5,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Census {
    pub cards: Vec<Card>,
    pub fault: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_contraction_is_unicode_safe_and_whitespace_canonical() {
        assert_eq!(snip("  alpha\n βeta   gamma ", 8), "alpha βe…");
    }

    #[test]
    fn pinned_heads_while_unpinned_stops_share_recency_and_closed_tails() {
        let card = |thread: &str, work, updated_at_ms| Card {
            harness: Harness::Codex,
            thread: thread.to_owned(),
            name: None,
            cwd: "/work".to_owned(),
            tile_preview: String::new(),
            work,
            window: (work != Work::Closed).then_some(1),
            workspace: None,
            updated_at_ms,
            pinned: false,
        };
        let mut cards = [
            card("old-sleep", Work::Sleep, 10),
            card("new-done", Work::Done, 20),
            card("newest-closed", Work::Closed, 30),
        ];
        cards[0].pinned = true;
        cards.sort();
        assert_eq!(
            cards.map(|card| card.thread),
            ["old-sleep", "new-done", "newest-closed"]
        );
    }
}

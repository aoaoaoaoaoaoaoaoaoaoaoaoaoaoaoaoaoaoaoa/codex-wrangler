use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::contract::{Harness, Work};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Retention {
    #[default]
    Active,
    Archived,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    pub harness: Harness,
    pub thread: String,
    pub name: Option<String>,
    pub cwd: String,
    pub tile_preview: String,
    pub work: Work,
    pub activity: Work,
    pub window: Option<u32>,
    pub workspace: Option<u32>,
    pub updated_at_ms: i64,
    pub retention: Retention,
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
        retention_rank(self)
            .cmp(&retention_rank(other))
            .then_with(|| rank(self.work).cmp(&rank(other.work)))
            .then_with(|| other.updated_at_ms.cmp(&self.updated_at_ms))
            .then_with(|| self.harness.cmp(&other.harness))
            .then_with(|| self.thread.cmp(&other.thread))
    }
}

const fn retention_rank(card: &Card) -> u8 {
    match (card.retention, card.window) {
        (Retention::Active, Some(_)) => 0,
        (Retention::Active, None) => 1,
        (Retention::Archived, _) => 2,
    }
}

impl PartialOrd for Card {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const fn rank(work: Work) -> u8 {
    match work {
        Work::Input => 0,
        Work::Goal => 1,
        Work::Turn => 2,
        Work::Sleeping => 3,
        Work::Done => 4,
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
}

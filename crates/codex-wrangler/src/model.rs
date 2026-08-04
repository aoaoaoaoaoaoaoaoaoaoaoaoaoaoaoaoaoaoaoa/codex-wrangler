use std::cmp::Ordering;

use crate::contract::Work;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCard {
    pub thread: String,
    pub name: Option<String>,
    pub cwd: String,
    pub tile_preview: String,
    pub work: Work,
    pub window: u32,
    pub workspace: Option<u32>,
    pub updated_at_ms: i64,
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

impl Ord for CodexCard {
    fn cmp(&self, other: &Self) -> Ordering {
        rank(self.work)
            .cmp(&rank(other.work))
            .then_with(|| other.updated_at_ms.cmp(&self.updated_at_ms))
            .then_with(|| self.thread.cmp(&other.thread))
    }
}

impl PartialOrd for CodexCard {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const fn rank(work: Work) -> u8 {
    match work {
        Work::Goal => 0,
        Work::Turn => 1,
        Work::Done => 2,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Census {
    pub cards: Vec<CodexCard>,
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

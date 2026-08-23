use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use codex_wrangler_contract::Harness;
use serde::{Deserialize, Serialize};

use crate::state;

const FILE: &str = "pinned-sessions.json";
const VERSION: u8 = 1;

#[derive(Default, Deserialize, Serialize)]
struct State {
    version: u8,
    pins: BTreeMap<Harness, BTreeSet<String>>,
}

pub struct Pinboard {
    path: PathBuf,
    pins: BTreeMap<Harness, BTreeSet<String>>,
    dirty: bool,
}

impl Pinboard {
    pub fn restore() -> Result<Self> {
        Self::restore_from(state::path(FILE)?)
    }

    pub fn contains(&self, harness: Harness, thread: &str) -> bool {
        self.pins
            .get(&harness)
            .is_some_and(|threads| threads.contains(thread))
    }

    pub fn set(&mut self, harness: Harness, thread: &str, pinned: bool) {
        let changed = if pinned {
            self.pins
                .entry(harness)
                .or_default()
                .insert(thread.to_owned())
        } else {
            let removed = self
                .pins
                .get_mut(&harness)
                .is_some_and(|threads| threads.remove(thread));
            if self.pins.get(&harness).is_some_and(BTreeSet::is_empty) {
                let _empty = self.pins.remove(&harness);
            }
            removed
        };
        self.dirty |= changed;
    }

    pub fn commit(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&State {
            version: VERSION,
            pins: self.pins.clone(),
        })?;
        state::seal(&self.path, &bytes)?;
        self.dirty = false;
        Ok(())
    }

    fn restore_from(path: PathBuf) -> Result<Self> {
        let pins = match fs::read(&path) {
            Ok(bytes) => {
                let state = serde_json::from_slice::<State>(&bytes)
                    .with_context(|| format!("decode `{}`", path.display()))?;
                anyhow::ensure!(
                    state.version == VERSION,
                    "unsupported pinboard state version {}",
                    state.version
                );
                state.pins
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error).with_context(|| format!("read `{}`", path.display())),
        };
        Ok(Self {
            path,
            pins,
            dirty: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinboard_roundtrip_preserves_harness_identity() {
        let root = tempfile::tempdir().expect("state root");
        let path = root.path().join("pinned-sessions.json");
        let mut pins = Pinboard::restore_from(path.clone()).expect("empty pinboard");
        pins.set(Harness::Codex, "same-thread", true);
        pins.commit().expect("seal pinboard");

        let pins = Pinboard::restore_from(path).expect("restore pinboard");
        assert!(pins.contains(Harness::Codex, "same-thread"));
        assert!(!pins.contains(Harness::PrimeAgent, "same-thread"));
    }
}

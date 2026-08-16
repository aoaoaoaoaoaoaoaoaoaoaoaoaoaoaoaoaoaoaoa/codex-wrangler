use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "preferences.json";
const VERSION: u8 = 1;

#[derive(Clone, Copy, Deserialize, Serialize)]
struct State {
    version: u8,
    confirm_deletion: bool,
    #[serde(default)]
    minimize_on_close: bool,
}

pub struct Preferences {
    path: Option<PathBuf>,
    confirm_deletion: bool,
    minimize_on_close: bool,
}

impl Preferences {
    pub fn restore() -> Self {
        let path = match crate::state::path(FILE) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("codex-wrangler cannot resolve its preferences: {error:#}");
                return Self {
                    path: None,
                    confirm_deletion: true,
                    minimize_on_close: false,
                };
            }
        };
        let restored = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<State>(&bytes)
                .ok()
                .filter(|state| state.version == VERSION),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                eprintln!(
                    "codex-wrangler cannot read preferences from `{}`: {error}",
                    path.display()
                );
                None
            }
        };
        Self {
            path: Some(path),
            confirm_deletion: restored.is_none_or(|state| state.confirm_deletion),
            minimize_on_close: restored.is_some_and(|state| state.minimize_on_close),
        }
    }

    pub const fn confirm_deletion(&self) -> bool {
        self.confirm_deletion
    }

    pub fn set_confirm_deletion(&mut self, confirm: bool) {
        if self.confirm_deletion == confirm {
            return;
        }
        self.confirm_deletion = confirm;
        self.persist();
    }

    pub const fn minimize_on_close(&self) -> bool {
        self.minimize_on_close
    }

    pub fn set_minimize_on_close(&mut self, minimize: bool) {
        if self.minimize_on_close == minimize {
            return;
        }
        self.minimize_on_close = minimize;
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let state = State {
            version: VERSION,
            confirm_deletion: self.confirm_deletion,
            minimize_on_close: self.minimize_on_close,
        };
        let result = serde_json::to_vec(&state)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| crate::state::seal(path, &bytes));
        if let Err(error) = result {
            eprintln!("codex-wrangler cannot save its preferences: {error:#}");
        }
    }
}

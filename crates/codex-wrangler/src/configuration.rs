use std::{
    fs,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use eternalist_apps::configuration::{
    Configuration as ConfigurationContract, ConfigurationFault, ConfigurationLedger,
};
use serde::{Deserialize, Serialize};

const LEGACY_FILE: &str = "preferences.json";
const LEGACY_VERSION: u8 = 1;
const SETTLE: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
struct ConfigurationValues {
    confirm_deletion: bool,
    minimize_on_close: bool,
}

impl Default for ConfigurationValues {
    fn default() -> Self {
        Self {
            confirm_deletion: true,
            minimize_on_close: false,
        }
    }
}

impl ConfigurationContract for ConfigurationValues {}

#[derive(Clone, Copy, Deserialize)]
struct Legacy {
    version: u8,
    confirm_deletion: bool,
    #[serde(default)]
    minimize_on_close: bool,
}

pub struct Configuration {
    ledger: ConfigurationLedger<ConfigurationValues>,
}

impl Configuration {
    pub fn raise(ctx: &egui::Context) -> Result<Self> {
        let project = ProjectDirs::from("moe", "Eternalist", "codex-wrangler")
            .context("cannot resolve the platform configuration directory")?;
        let path = project.config_dir().join("config.toml");
        let fallback = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                legacy().unwrap_or_else(|error| {
                    eprintln!("codex-wrangler cannot migrate its former preferences: {error:#}");
                    ConfigurationValues::default()
                })
            }
            Ok(_) | Err(_) => ConfigurationValues::default(),
        };
        let ledger = ConfigurationLedger::raise_with_fallback(
            "codex-wrangler-configuration",
            ctx,
            path,
            SETTLE,
            fallback,
        )?;
        Ok(Self { ledger })
    }

    pub const fn confirm_deletion(&self) -> bool {
        self.ledger.live().confirm_deletion
    }

    pub fn set_confirm_deletion(&mut self, confirm: bool) -> bool {
        self.ledger
            .revise(|values| values.confirm_deletion = confirm)
            .unwrap_or_else(|error| {
                eprintln!("codex-wrangler cannot revise delete confirmation: {error:#}");
                false
            })
    }

    pub const fn minimize_on_close(&self) -> bool {
        self.ledger.live().minimize_on_close
    }

    pub fn set_minimize_on_close(&mut self, minimize: bool) -> bool {
        self.ledger
            .revise(|values| values.minimize_on_close = minimize)
            .unwrap_or_else(|error| {
                eprintln!("codex-wrangler cannot revise close behavior: {error:#}");
                false
            })
    }

    pub const fn writable(&self) -> bool {
        self.ledger.writable()
    }

    pub fn path(&self) -> &std::path::Path {
        self.ledger.path()
    }

    pub const fn fault(&self) -> Option<&ConfigurationFault> {
        self.ledger.fault()
    }

    pub const fn reload_pending(&self) -> bool {
        self.ledger.reload_pending()
    }

    pub fn settled(&self) -> bool {
        self.ledger.settled()
    }

    pub fn request_reload(&mut self) -> bool {
        self.ledger.request_reload().unwrap_or_else(|error| {
            eprintln!("codex-wrangler cannot reload its configuration: {error:#}");
            false
        })
    }

    pub fn absorb(&mut self) -> bool {
        self.ledger.absorb()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.ledger.deadline()
    }

    pub fn service_deadline_reached(&mut self, now: Instant) -> bool {
        self.ledger.service_deadline_reached(now)
    }
}

fn legacy() -> Result<ConfigurationValues> {
    let path = crate::state::path(LEGACY_FILE)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigurationValues::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read `{}`", path.display())),
    };
    let legacy = serde_json::from_slice::<Legacy>(&bytes)
        .with_context(|| format!("decode `{}`", path.display()))?;
    anyhow::ensure!(
        legacy.version == LEGACY_VERSION,
        "unsupported legacy preference version {}",
        legacy.version
    );
    Ok(ConfigurationValues {
        confirm_deletion: legacy.confirm_deletion,
        minimize_on_close: legacy.minimize_on_close,
    })
}

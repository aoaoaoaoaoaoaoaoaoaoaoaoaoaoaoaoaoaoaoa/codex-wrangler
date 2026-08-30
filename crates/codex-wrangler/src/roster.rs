use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::state;

const FILE: &str = "known-sessions.json";
const VERSION: u8 = 2;
const RESET_SLOP_SECS: i64 = 5;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccountMark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    quotas: BTreeSet<QuotaMark>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct QuotaMark {
    pub limit: String,
    pub window_minutes: i64,
    pub resets_at: i64,
}

impl AccountMark {
    pub fn current(account: Option<String>, quotas: impl IntoIterator<Item = QuotaMark>) -> Self {
        Self {
            account,
            quotas: quotas.into_iter().collect(),
        }
    }

    pub fn quota(quota: QuotaMark) -> Self {
        Self::current(None, [quota])
    }

    pub fn rotated_to(&self, current: &Self) -> bool {
        if let (Some(bound), Some(active)) = (&self.account, &current.account) {
            return bound != active;
        }
        let mut comparable = false;
        for bound in &self.quotas {
            for active in current
                .quotas
                .iter()
                .filter(|active| active.limit == bound.limit)
            {
                comparable = true;
                if bound.compatible(active) {
                    return false;
                }
            }
        }
        comparable
    }

    fn absorb(&mut self, observed: Self) {
        if observed.account.is_some() && self.rotated_to(&observed) {
            self.account = observed.account;
            self.quotas = observed.quotas;
            return;
        }
        if observed.account.is_some() {
            self.account = observed.account;
        }
        for quota in observed.quotas {
            self.quotas.retain(|prior| prior.limit != quota.limit);
            let _new = self.quotas.insert(quota);
        }
    }
}

impl QuotaMark {
    fn compatible(&self, other: &Self) -> bool {
        if self.window_minutes != other.window_minutes || self.window_minutes <= 0 {
            return false;
        }
        let period = self.window_minutes.saturating_mul(60);
        let phase = self.resets_at.abs_diff(other.resets_at) % u64::try_from(period).unwrap_or(1);
        phase <= RESET_SLOP_SECS.cast_unsigned()
            || period.cast_unsigned() - phase <= RESET_SLOP_SECS.cast_unsigned()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SeenSession {
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub preview: String,
    pub updated_at_ms: i64,
    pub workspace: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    #[serde(default, skip_serializing_if = "AccountMark::is_empty")]
    pub account: AccountMark,
}

impl AccountMark {
    fn is_empty(&self) -> bool {
        self.account.is_none() && self.quotas.is_empty()
    }
}

pub struct Sighting<'a> {
    pub thread: &'a str,
    pub name: Option<&'a str>,
    pub cwd: &'a Path,
    pub preview: &'a str,
    pub updated_at_ms: i64,
    pub workspace: Option<u32>,
    pub cli_version: Option<&'a str>,
    pub account: Option<AccountMark>,
}

#[derive(Default, Deserialize, Serialize)]
struct State {
    version: u8,
    sessions: BTreeMap<String, SeenSession>,
}

pub struct Roster {
    path: PathBuf,
    sessions: BTreeMap<String, SeenSession>,
    dirty: bool,
}

impl Roster {
    pub fn restore() -> Result<Self> {
        Self::restore_from(state::path(FILE)?)
    }

    pub fn sight(&mut self, sighting: Sighting<'_>) {
        let candidate = SeenSession {
            name: sighting.name.map(str::to_owned),
            cwd: sighting.cwd.to_owned(),
            preview: sighting.preview.to_owned(),
            updated_at_ms: sighting.updated_at_ms,
            workspace: sighting.workspace,
            cli_version: sighting.cli_version.map(str::to_owned),
            account: sighting.account.unwrap_or_default(),
        };
        if let Some(session) = self.sessions.get_mut(sighting.thread) {
            let prior = session.clone();
            session.name = candidate.name;
            session.cwd = candidate.cwd;
            session.preview = candidate.preview;
            session.updated_at_ms = candidate.updated_at_ms;
            session.workspace = candidate.workspace.or(session.workspace);
            if session.cli_version.is_none() {
                session.cli_version = candidate.cli_version;
            }
            session.account.absorb(candidate.account);
            self.dirty |= *session != prior;
        } else {
            let _prior = self.sessions.insert(sighting.thread.to_owned(), candidate);
            self.dirty = true;
        }
    }

    pub fn bind(&mut self, thread: &str, account: AccountMark) {
        if let Some(session) = self.sessions.get_mut(thread)
            && session.account != account
        {
            session.account = account;
            self.dirty = true;
        }
    }

    pub fn bind_version(&mut self, thread: &str, version: &str) {
        if let Some(session) = self.sessions.get_mut(thread)
            && session.cli_version.as_deref() != Some(version)
        {
            session.cli_version = Some(version.to_owned());
            self.dirty = true;
        }
    }

    pub fn forget(&mut self, thread: &str) {
        self.dirty |= self.sessions.remove(thread).is_some();
    }

    pub fn get(&self, thread: &str) -> Option<&SeenSession> {
        self.sessions.get(thread)
    }

    pub fn sessions(&self) -> impl Iterator<Item = (&str, &SeenSession)> {
        self.sessions
            .iter()
            .map(|(thread, session)| (thread.as_str(), session))
    }

    pub fn commit(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&State {
            version: VERSION,
            sessions: self.sessions.clone(),
        })?;
        state::seal(&self.path, &bytes)?;
        self.dirty = false;
        Ok(())
    }

    fn restore_from(path: PathBuf) -> Result<Self> {
        let (sessions, dirty) = match fs::read(&path) {
            Ok(bytes) => {
                let state = serde_json::from_slice::<State>(&bytes)
                    .with_context(|| format!("decode `{}`", path.display()))?;
                match state.version {
                    VERSION => (state.sessions, false),
                    1 => (BTreeMap::new(), true),
                    version => anyhow::bail!("unsupported known-session state version {version}"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (BTreeMap::new(), false),
            Err(error) => return Err(error).with_context(|| format!("read `{}`", path.display())),
        };
        Ok(Self {
            path,
            sessions,
            dirty,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn quota(reset: i64) -> AccountMark {
        AccountMark::quota(QuotaMark {
            limit: "codex".to_owned(),
            window_minutes: 10_080,
            resets_at: reset,
        })
    }

    #[test]
    fn quota_phase_survives_window_rollover_but_rejects_another_plan() {
        let week = 10_080 * 60;
        assert!(!quota(1_000_000).rotated_to(&quota(1_000_000 + week + 3)));
        assert!(quota(1_000_000).rotated_to(&quota(1_000_300)));
    }

    #[test]
    fn stale_rollout_quota_cannot_erase_an_explicit_account_binding() {
        let mut bound = AccountMark::current(
            Some("new-account".to_owned()),
            [QuotaMark {
                limit: "codex".to_owned(),
                window_minutes: 10_080,
                resets_at: 2_000_000,
            }],
        );
        bound.absorb(quota(1_000_000));
        assert_eq!(bound.account.as_deref(), Some("new-account"));
        assert_eq!(bound.quotas, quota(1_000_000).quotas);
    }

    #[test]
    fn v2_roundtrip_is_private_atomic_state() {
        let root = tempfile::tempdir().expect("state root");
        let path = root.path().join("codex-wrangler/known-sessions.json");
        let mut roster = Roster::restore_from(path.clone()).expect("empty roster");
        roster.sight(Sighting {
            thread: "thread-1",
            name: Some("Cold engine"),
            cwd: Path::new("/work/cold"),
            preview: "Still.",
            updated_at_ms: 7,
            workspace: Some(4),
            cli_version: Some("0.149.0"),
            account: Some(quota(1_000_000)),
        });
        roster.commit().expect("seal roster");
        assert_eq!(
            fs::metadata(&path)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let restored = Roster::restore_from(path.clone()).expect("restore roster");
        assert_eq!(
            restored
                .get("thread-1")
                .and_then(|session| session.workspace),
            Some(4)
        );
        let state = serde_json::from_slice::<State>(&fs::read(&path).expect("read state"))
            .expect("decode state");
        assert_eq!(state.version, VERSION);
        assert!(!path.with_file_name(".known-sessions.json.tmp").exists());
    }

    #[test]
    fn v1_membership_is_reset_and_committed_as_empty_v2() {
        let root = tempfile::tempdir().expect("state root");
        let path = root.path().join("known-sessions.json");
        fs::write(
            &path,
            br#"{
                "version": 1,
                "sessions": {
                    "ancient-thread": {
                        "name": "CODER_EULER",
                        "cwd": "/work/ancient",
                        "preview": "lost provenance",
                        "updated_at_ms": 1,
                        "workspace": 9,
                        "retention": "archived"
                    }
                }
            }"#,
        )
        .expect("write v1 state");

        let mut roster = Roster::restore_from(path.clone()).expect("quarantine v1 roster");
        assert!(roster.sessions().next().is_none());
        roster.commit().expect("seal empty v2 roster");

        let state = serde_json::from_slice::<State>(&fs::read(path).expect("read v2 state"))
            .expect("decode v2 state");
        assert_eq!(state.version, VERSION);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn forgetting_a_closed_session_removes_only_roster_state() {
        let root = tempfile::tempdir().expect("state root");
        let path = root.path().join("known-sessions.json");
        let mut roster = Roster::restore_from(path.clone()).expect("empty roster");
        roster.sight(Sighting {
            thread: "thread-1",
            name: None,
            cwd: Path::new("/work"),
            preview: "",
            updated_at_ms: 0,
            workspace: None,
            cli_version: None,
            account: None,
        });
        roster.commit().expect("seal sighting");
        roster.forget("thread-1");
        roster.commit().expect("seal forgetting");
        assert!(roster.get("thread-1").is_none());
    }
}

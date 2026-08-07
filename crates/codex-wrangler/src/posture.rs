use std::{fs, path::PathBuf};

const FILE: &str = "window-mode";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Posture {
    #[default]
    Floating,
    Tiled,
}

pub struct Ledger {
    path: Option<PathBuf>,
    remembered: Option<Posture>,
}

impl Posture {
    pub const fn from_floating(floating: bool) -> Self {
        if floating {
            Self::Floating
        } else {
            Self::Tiled
        }
    }

    pub const fn floating(self) -> bool {
        matches!(self, Self::Floating)
    }

    const fn text(self) -> &'static str {
        match self {
            Self::Floating => "floating\n",
            Self::Tiled => "tiled\n",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "floating" => Some(Self::Floating),
            "tiled" => Some(Self::Tiled),
            _ => None,
        }
    }
}

impl Ledger {
    pub fn restore() -> (Self, Posture) {
        let path = match crate::state::path(FILE) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("codex-wrangler cannot resolve its XDG state: {error:#}");
                return (
                    Self {
                        path: None,
                        remembered: None,
                    },
                    Posture::default(),
                );
            }
        };
        Self::restore_from(path)
    }

    pub fn remember(&mut self, posture: Posture) {
        if self.remembered == Some(posture) {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        match crate::state::seal(path, posture.text().as_bytes()) {
            Ok(()) => self.remembered = Some(posture),
            Err(error) => {
                eprintln!("codex-wrangler cannot save its window mode: {error:#}");
            }
        }
    }

    fn restore_from(path: PathBuf) -> (Self, Posture) {
        let restored = match fs::read_to_string(&path) {
            Ok(text) => {
                if let Some(posture) = Posture::parse(&text) {
                    Some(posture)
                } else {
                    eprintln!(
                        "codex-wrangler ignored invalid window mode in `{}`",
                        path.display()
                    );
                    None
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                eprintln!(
                    "codex-wrangler cannot read window mode from `{}`: {error}",
                    path.display()
                );
                None
            }
        };
        (
            Self {
                path: Some(path),
                remembered: restored,
            },
            restored.unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn posture_round_trips_through_private_atomic_state() {
        let root = tempfile::tempdir().expect("temporary state root");
        let path = root.path().join("codex-wrangler").join(FILE);
        let (mut ledger, restored) = Ledger::restore_from(path.clone());
        assert_eq!(restored, Posture::Floating);

        ledger.remember(Posture::Tiled);
        assert_eq!(
            fs::read_to_string(&path).expect("persisted mode"),
            "tiled\n"
        );
        assert_eq!(
            fs::metadata(path.parent().expect("state parent"))
                .expect("state parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let (_ledger, restored) = Ledger::restore_from(path);
        assert_eq!(restored, Posture::Tiled);
    }
}

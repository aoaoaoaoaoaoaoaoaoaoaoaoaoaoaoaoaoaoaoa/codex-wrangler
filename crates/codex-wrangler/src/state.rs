use std::{
    env, fs,
    io::Write as _,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};

const APPLICATION: &str = "codex-wrangler";

pub fn path(file: &str) -> Result<PathBuf> {
    path_from(
        file,
        env::var_os("XDG_STATE_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
    .context("neither absolute XDG_STATE_HOME nor HOME is available")
}

pub fn seal(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("XDG state path has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("create `{}`", parent.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("XDG state path has no UTF-8 filename")?;
    let temporary = path.with_file_name(format!(".{name}.tmp"));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("stage `{}`", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write `{}`", path.display()))?;
    file.sync_all()
        .with_context(|| format!("seal `{}`", path.display()))?;
    drop(file);
    fs::rename(&temporary, path).with_context(|| format!("publish `{}`", path.display()))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("seal `{}`", parent.display()))
}

pub(crate) fn path_from(
    file: &str,
    xdg: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    xdg.map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.map(Path::new)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .map(|root| root.join(APPLICATION).join(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_xdg_state_is_banished_to_the_absolute_home_default() {
        assert_eq!(
            path_from(
                "window-mode",
                Some("relative".as_ref()),
                Some("/home/keeper".as_ref())
            ),
            Some(PathBuf::from(
                "/home/keeper/.local/state/codex-wrangler/window-mode"
            ))
        );
        assert_eq!(
            path_from(
                "window-mode",
                Some("/vault/state".as_ref()),
                Some("/home/keeper".as_ref())
            ),
            Some(PathBuf::from("/vault/state/codex-wrangler/window-mode"))
        );
    }
}

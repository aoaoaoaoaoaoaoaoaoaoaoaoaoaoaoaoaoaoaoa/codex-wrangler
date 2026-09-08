use std::{
    fs,
    io::Write as _,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use eternalist_apps::ApplicationPaths;

pub fn path(file: &str) -> Result<PathBuf> {
    Ok(ApplicationPaths::claim(crate::PRODUCT)?.state.join(file))
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

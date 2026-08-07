use std::{
    collections::{HashMap, HashSet},
    io,
    os::fd::{AsFd as _, BorrowedFd},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

const BUFFER_BYTES: usize = 64 << 10;
const FILE_EVENTS: WatchMask = WatchMask::MODIFY
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::ATTRIB)
    .union(WatchMask::MOVE_SELF)
    .union(WatchMask::DELETE_SELF);
const DIRECTORY_EVENTS: WatchMask = WatchMask::CLOSE_WRITE
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVE)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

#[derive(Debug, Default)]
pub struct Flare {
    pub paths: HashSet<PathBuf>,
    pub overflowed: bool,
}

pub struct Watchfire {
    inotify: Inotify,
    paths: HashMap<PathBuf, WatchDescriptor>,
    roots: HashMap<WatchDescriptor, HashSet<PathBuf>>,
    buffer: Vec<u8>,
}

impl Watchfire {
    pub fn kindle() -> Result<Self> {
        Ok(Self {
            inotify: Inotify::init().context("create inotify watchfire")?,
            paths: HashMap::new(),
            roots: HashMap::new(),
            buffer: vec![0; BUFFER_BYTES],
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    pub fn reconcile(&mut self, files: impl IntoIterator<Item = PathBuf>) -> Result<()> {
        let desired = files
            .into_iter()
            .flat_map(|file| {
                let parent = file.parent().map(Path::to_path_buf);
                std::iter::once(file).chain(parent)
            })
            .filter(|path| path.exists())
            .collect::<HashSet<_>>();

        let stale = self
            .paths
            .keys()
            .filter(|path| !desired.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in stale {
            self.remove(&path);
        }
        for path in desired {
            if self.paths.contains_key(&path) {
                continue;
            }
            let mask = if path.is_dir() {
                DIRECTORY_EVENTS
            } else {
                FILE_EVENTS
            };
            let descriptor = self
                .inotify
                .watches()
                .add(&path, mask)
                .with_context(|| format!("watch `{}`", path.display()))?;
            let _prior = self.paths.insert(path.clone(), descriptor.clone());
            let _new = self.roots.entry(descriptor).or_default().insert(path);
        }
        Ok(())
    }

    pub fn reap(&mut self) -> Result<Flare> {
        let mut flare = Flare::default();
        loop {
            let events = match self.inotify.read_events(&mut self.buffer) {
                Ok(events) => events,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("read inotify watchfire"),
            };
            let mut ignored = HashSet::new();
            for event in events {
                flare.overflowed |= event.mask.contains(EventMask::Q_OVERFLOW);
                if event.mask.contains(EventMask::IGNORED) {
                    let _new = ignored.insert(event.wd.clone());
                }
                if let Some(roots) = self.roots.get(&event.wd) {
                    flare.paths.extend(roots.iter().map(|root| {
                        event.name.map_or_else(
                            || root.clone(),
                            |name| {
                                if root.is_dir() {
                                    root.join(name)
                                } else {
                                    root.clone()
                                }
                            },
                        )
                    }));
                }
            }
            for descriptor in ignored {
                self.forget(&descriptor);
            }
        }
        Ok(flare)
    }

    fn remove(&mut self, path: &Path) {
        let Some(descriptor) = self.paths.remove(path) else {
            return;
        };
        let vacant = self.roots.get_mut(&descriptor).is_some_and(|roots| {
            let _removed = roots.remove(path);
            roots.is_empty()
        });
        if vacant {
            let _roots = self.roots.remove(&descriptor);
            let _removed = self.inotify.watches().remove(descriptor);
        }
    }

    fn forget(&mut self, descriptor: &WatchDescriptor) {
        let Some(paths) = self.roots.remove(descriptor) else {
            return;
        };
        for path in paths {
            let _descriptor = self.paths.remove(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write as _, thread, time::Duration};

    use super::*;

    #[test]
    fn file_replacement_and_append_both_flare() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("rollout.jsonl");
        fs::write(&path, b"first\n").expect("seed file");
        let mut fire = Watchfire::kindle().expect("watchfire");
        fire.reconcile([path.clone()]).expect("watch file");

        let mut transcript = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append");
        writeln!(transcript, "second").expect("append");
        drop(transcript);
        assert!(await_path(&mut fire, &path));

        fs::write(&path, b"replacement\n").expect("replace file");
        assert!(await_path(&mut fire, &path));
    }

    fn await_path(fire: &mut Watchfire, path: &Path) -> bool {
        for _attempt in 0..100 {
            if fire
                .reap()
                .expect("reap")
                .paths
                .iter()
                .any(|changed| changed == path)
            {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }
}

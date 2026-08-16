use std::{collections::HashMap, fs, path::Path, time::SystemTime};

use anyhow::{Context as _, Result};

#[derive(Default)]
pub struct NameIndex {
    stamp: Option<(u64, SystemTime)>,
    names: HashMap<String, String>,
}

impl NameIndex {
    pub fn refresh(&mut self, path: &Path) -> Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.stamp = None;
                self.names.clear();
                return Ok(());
            }
            Err(error) => return Err(error).context("inspect Codex session-name index"),
        };
        let stamp = (metadata.len(), metadata.modified()?);
        if self.stamp == Some(stamp) {
            return Ok(());
        }
        self.names = parse(&fs::read(path)?);
        self.stamp = Some(stamp);
        Ok(())
    }

    pub fn get(&self, thread: &str) -> Option<&str> {
        self.names.get(thread).map(String::as_str)
    }
}

fn parse(bytes: &[u8]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(thread) = record.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(name) = record
            .get("thread_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
        else {
            continue;
        };
        if name.is_empty() {
            names.remove(thread);
        } else {
            let _prior = names.insert(thread.to_owned(), name.to_owned());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_nonempty_name_is_authoritative() {
        let names = parse(
            br#"{"id":"named","thread_name":"first"}
{"id":"anonymous","thread_name":""}
{"id":"named","thread_name":"final"}
"#,
        );
        assert_eq!(names.get("named").map(String::as_str), Some("final"));
        assert!(!names.contains_key("anonymous"));
    }
}

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use codex_wrangler_contract::{Harness, Work};

const SCAN_LIMIT: u64 = 4 << 20;
const RECORD_LIMIT: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct Summary {
    pub cwd: Option<PathBuf>,
    pub name: Option<String>,
    pub preview: String,
    pub updated_at_ms: i64,
    pub work: Work,
}

#[derive(Clone, Debug)]
struct Pulse {
    cwd: Option<PathBuf>,
    name: Option<String>,
    name_rank: u8,
    preview: String,
    work: Work,
}

impl Default for Pulse {
    fn default() -> Self {
        Self {
            cwd: None,
            name: None,
            name_rank: 0,
            preview: String::new(),
            work: Work::Done,
        }
    }
}

impl Pulse {
    fn absorb(&mut self, harness: Harness, line: &[u8]) {
        let Some(record) = decode(line) else {
            return;
        };
        self.absorb_identity(harness, &record);
        match harness {
            Harness::Codex => {}
            Harness::ClaudeCode => self.absorb_claude(&record),
            Harness::PrimeAgent => self.absorb_prime(&record),
        }
    }

    fn absorb_identity_line(&mut self, harness: Harness, line: &[u8]) {
        if let Some(record) = decode(line) {
            self.absorb_identity(harness, &record);
        }
    }

    fn absorb_identity(&mut self, harness: Harness, record: &Value) {
        match (harness, record.get("type").and_then(Value::as_str)) {
            (Harness::ClaudeCode, Some("custom-title")) => self.assign_name(
                record
                    .get("customTitle")
                    .or_else(|| record.get("title"))
                    .and_then(Value::as_str),
                2,
            ),
            (Harness::ClaudeCode, Some("ai-title")) => {
                self.assign_name(record.get("aiTitle").and_then(Value::as_str), 1);
            }
            (Harness::ClaudeCode, Some("user" | "assistant")) if !sidechain(record) => {
                self.assign_cwd(record);
            }
            (Harness::PrimeAgent, Some("session")) => self.assign_cwd(record),
            (Harness::PrimeAgent, Some("session_info")) => {
                self.assign_name(record.get("name").and_then(Value::as_str), 2);
            }
            _ => {}
        }
    }

    fn absorb_claude(&mut self, record: &Value) {
        let kind = record.get("type").and_then(Value::as_str);
        match kind {
            Some("user") if !sidechain(record) => {
                let message = &record["message"];
                let meta = record.get("isMeta").and_then(Value::as_bool) == Some(true);
                if !meta {
                    if let Some(text) = message_text(message) {
                        assign_text(&mut self.preview, &text);
                    }
                    self.work = Work::Turn;
                }
            }
            Some("assistant") if !sidechain(record) => {
                let message = &record["message"];
                if let Some(text) = message_text(message) {
                    assign_text(&mut self.preview, &text);
                }
                self.work = if asks_user(message) {
                    Work::Input
                } else {
                    match message.get("stop_reason").and_then(Value::as_str) {
                        Some("end_turn" | "stop_sequence" | "stop") => Work::Done,
                        _ => Work::Turn,
                    }
                };
            }
            Some("system")
                if record.get("subtype").and_then(Value::as_str) == Some("turn_duration") =>
            {
                self.work = Work::Done;
            }
            _ => {}
        }
    }

    fn absorb_prime(&mut self, record: &Value) {
        match record.get("type").and_then(Value::as_str) {
            Some("message") => {
                let message = &record["message"];
                match message.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        if let Some(text) = message_text(message) {
                            assign_text(&mut self.preview, &text);
                        }
                        self.work = Work::Turn;
                    }
                    Some("assistant") => {
                        if let Some(text) = message_text(message) {
                            assign_text(&mut self.preview, &text);
                        }
                        self.work = match message.get("stopReason").and_then(Value::as_str) {
                            Some("stop" | "endTurn" | "end_turn") => Work::Done,
                            _ => Work::Turn,
                        };
                    }
                    Some("toolResult") => self.work = Work::Turn,
                    _ => {}
                }
            }
            Some("agent_status") => {
                self.work = match record["status"].get("taskState").and_then(Value::as_str) {
                    Some("completed") => Work::Done,
                    Some("needs_input") => Work::Input,
                    _ => self.work,
                };
            }
            _ => {}
        }
    }

    fn assign_cwd(&mut self, record: &Value) {
        if let Some(cwd) = record
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
        {
            self.cwd = Some(PathBuf::from(cwd));
        }
    }

    fn assign_name(&mut self, name: Option<&str>, rank: u8) {
        if rank < self.name_rank {
            return;
        }
        self.name_rank = rank;
        self.name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
    }
}

#[derive(Clone, Debug)]
struct Memo {
    length: u64,
    modified: SystemTime,
    pulse: Pulse,
}

#[derive(Default)]
pub struct Transcripts {
    memo: HashMap<(Harness, PathBuf), Memo>,
}

impl Transcripts {
    pub fn read(&mut self, harness: Harness, path: &Path) -> std::io::Result<Summary> {
        debug_assert_ne!(harness, Harness::Codex);
        let metadata = fs::metadata(path)?;
        let length = metadata.len();
        let modified = metadata.modified()?;
        let key = (harness, path.to_owned());
        let pulse = match self.memo.get(&key) {
            Some(memo) if memo.length == length && memo.modified == modified => memo.pulse.clone(),
            Some(memo) if memo.length < length && length - memo.length <= SCAN_LIMIT => {
                let mut pulse = memo.pulse.clone();
                absorb_range(path, memo.length, harness, &mut pulse)?;
                pulse
            }
            _ => scan_tail(path, length, harness)?,
        };
        let updated_at_ms = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX);
        let summary = Summary {
            cwd: pulse.cwd.clone(),
            name: pulse.name.clone(),
            preview: pulse.preview.clone(),
            updated_at_ms,
            work: pulse.work,
        };
        let _prior = self.memo.insert(
            key,
            Memo {
                length,
                modified,
                pulse,
            },
        );
        Ok(summary)
    }
}

fn scan_tail(path: &Path, length: u64, harness: Harness) -> std::io::Result<Pulse> {
    let mut pulse = Pulse::default();
    if length > SCAN_LIMIT {
        let mut header = File::open(path)?.take(RECORD_LIMIT as u64);
        let mut bytes = Vec::new();
        let _read = header.read_to_end(&mut bytes)?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        absorb_identity_lines(&bytes[..end], harness, &mut pulse);
    }
    let start = length.saturating_sub(SCAN_LIMIT);
    let mut file = File::open(path)?;
    let _position = file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(length - start).unwrap_or_default());
    let _read = file.read_to_end(&mut bytes)?;
    let begin = if start == 0 {
        0
    } else {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    };
    absorb_lines(&bytes[begin..], harness, &mut pulse);
    Ok(pulse)
}

fn absorb_range(
    path: &Path,
    start: u64,
    harness: Harness,
    pulse: &mut Pulse,
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let _position = file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    let _read = file.read_to_end(&mut bytes)?;
    absorb_lines(&bytes, harness, pulse);
    Ok(())
}

fn absorb_lines(bytes: &[u8], harness: Harness, pulse: &mut Pulse) {
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        pulse.absorb(harness, line);
    }
}

fn absorb_identity_lines(bytes: &[u8], harness: Harness, pulse: &mut Pulse) {
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        pulse.absorb_identity_line(harness, line);
    }
}

fn decode(line: &[u8]) -> Option<Value> {
    if line.len() > RECORD_LIMIT {
        return None;
    }
    serde_json::from_slice(line).ok()
}

fn sidechain(record: &Value) -> bool {
    record.get("isSidechain").and_then(Value::as_bool) == Some(true)
}

fn asks_user(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block.get("name").and_then(Value::as_str) == Some("AskUserQuestion")
            })
        })
}

fn message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return (!text.trim().is_empty()).then(|| text.to_owned());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn assign_text(slot: &mut String, text: &str) {
    if !text.trim().is_empty() {
        text.trim().clone_into(slot);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn summarize(harness: Harness, lines: &[&str]) -> Summary {
        let mut file = tempfile::NamedTempFile::new().expect("fixture");
        for line in lines {
            writeln!(file, "{line}").expect("write fixture");
        }
        Transcripts::default()
            .read(harness, file.path())
            .expect("summarize")
    }

    #[test]
    fn claude_title_preview_and_completed_turn_survive_noise() {
        let summary = summarize(
            Harness::ClaudeCode,
            &[
                r#"{"type":"ai-title","sessionId":"c","aiTitle":"Invader calculus"}"#,
                r#"{"type":"user","cwd":"/work/claude","message":{"role":"user","content":"Cut it."}}"#,
                r#"{"type":"assistant","cwd":"/work/claude","message":{"role":"assistant","content":[{"type":"text","text":"It is cut."}],"stop_reason":"end_turn"}}"#,
                r#"{"type":"system","subtype":"turn_duration"}"#,
            ],
        );
        assert_eq!(summary.name.as_deref(), Some("Invader calculus"));
        assert_eq!(summary.cwd.as_deref(), Some(Path::new("/work/claude")));
        assert_eq!(summary.preview, "It is cut.");
        assert_eq!(summary.work, Work::Done);
    }

    #[test]
    fn claude_ask_user_is_input_not_generic_work() {
        let summary = summarize(
            Harness::ClaudeCode,
            &[
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"AskUserQuestion"}],"stop_reason":"tool_use"}}"#,
            ],
        );
        assert_eq!(summary.work, Work::Input);
    }

    #[test]
    fn prime_name_and_verdict_are_native_records() {
        let summary = summarize(
            Harness::PrimeAgent,
            &[
                r#"{"type":"session","id":"p","cwd":"/work/prime"}"#,
                r#"{"type":"session_info","name":"Butterfly siege"}"#,
                r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"Begin."}]}}"#,
                r#"{"type":"agent_status","status":{"taskState":"needs_input"}}"#,
            ],
        );
        assert_eq!(summary.name.as_deref(), Some("Butterfly siege"));
        assert_eq!(summary.preview, "Begin.");
        assert_eq!(summary.work, Work::Input);
    }

    #[test]
    fn cold_tail_scan_preserves_identity_from_a_bounded_prefix() {
        let mut file = tempfile::NamedTempFile::new().expect("fixture");
        writeln!(file, r#"{{"type":"ai-title","aiTitle":"Ancient invader"}}"#)
            .expect("write title");
        writeln!(
            file,
            r#"{{"type":"user","cwd":"/work/ancient","message":{{"role":"user","content":"Begin."}}}}"#
        )
        .expect("write cwd");
        file.seek(SeekFrom::Start(SCAN_LIMIT + 1_024))
            .expect("raise sparse gulf");
        writeln!(file).expect("terminate sparse gulf");
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"Finished.","stop_reason":"end_turn"}}}}"#
        )
        .expect("write tail");

        let summary = Transcripts::default()
            .read(Harness::ClaudeCode, file.path())
            .expect("summarize");
        assert_eq!(summary.name.as_deref(), Some("Ancient invader"));
        assert_eq!(summary.cwd.as_deref(), Some(Path::new("/work/ancient")));
        assert_eq!(summary.preview, "Finished.");
        assert_eq!(summary.work, Work::Done);
    }
}

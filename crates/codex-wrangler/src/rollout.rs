use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

use memchr::memmem;
use serde_json::Value;

use crate::roster::{AccountMark, QuotaMark};

const BLOCK: usize = 1 << 20;
const ACCOUNT_HORIZON: u64 = 4 << 20;
const INPUT_REQUEST: &[u8] = b"\"name\":\"request_user_input\"";
const CALL_OUTPUT: &[u8] = b"\"type\":\"function_call_output\"";
const CALL_ID: &[u8] = b"\"call_id\":\"";

#[derive(Clone, Debug, Default)]
struct Pulse {
    running: bool,
    input_call: Option<String>,
    preview: String,
    account: Option<AccountMark>,
}

impl Pulse {
    fn absorb(&mut self, line: &[u8]) {
        if memmem::find(line, INPUT_REQUEST).is_some() {
            self.input_call = call_id(line).map(str::to_owned);
            return;
        }
        if self.input_call.is_some()
            && memmem::find(line, CALL_OUTPUT).is_some()
            && call_id(line) == self.input_call.as_deref()
        {
            self.input_call = None;
            return;
        }
        if !interesting(line) {
            return;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let payload = &event["payload"];
        if event.get("type").and_then(Value::as_str) != Some("event_msg") {
            return;
        }
        match payload.get("type").and_then(Value::as_str) {
            Some("token_count") => {
                self.account = payload
                    .get("rate_limits")
                    .and_then(quota)
                    .map(AccountMark::quota);
            }
            Some("task_started" | "turn_started") => self.running = true,
            Some("task_complete" | "turn_complete" | "turn_aborted") => {
                self.running = false;
                self.input_call = None;
                if let Some(message) = payload.get("last_agent_message").and_then(Value::as_str) {
                    assign_preview(&mut self.preview, message);
                }
            }
            Some("user_message" | "agent_message") => {
                if let Some(message) = payload.get("message").and_then(Value::as_str) {
                    assign_preview(&mut self.preview, message);
                }
            }
            _ => {}
        }
    }
}

fn assign_preview(slot: &mut String, message: &str) {
    if !message.trim().is_empty() {
        message.trim().clone_into(slot);
    }
}

fn interesting(line: &[u8]) -> bool {
    [
        b"\"type\":\"task_".as_slice(),
        b"\"type\":\"turn_".as_slice(),
        b"\"type\":\"user_message\"".as_slice(),
        b"\"type\":\"agent_message\"".as_slice(),
        b"\"type\":\"token_count\"".as_slice(),
    ]
    .iter()
    .any(|needle| memmem::find(line, needle).is_some())
}

fn call_id(line: &[u8]) -> Option<&str> {
    let start = memmem::find(line, CALL_ID)? + CALL_ID.len();
    let tail = line.get(start..)?;
    let end = memchr::memchr(b'"', tail)?;
    std::str::from_utf8(&tail[..end]).ok()
}

#[derive(Clone, Debug)]
struct Memo {
    length: u64,
    pulse: Pulse,
}

#[derive(Default)]
pub struct Rollouts {
    memo: HashMap<PathBuf, Memo>,
}

#[derive(Clone, Debug)]
pub struct RolloutSummary {
    pub preview: String,
    pub running: bool,
    pub waiting_for_input: bool,
    pub account: Option<AccountMark>,
}

impl Rollouts {
    pub fn read(&mut self, path: &Path) -> std::io::Result<RolloutSummary> {
        let length = fs::metadata(path)?.len();
        let pulse = match self.memo.get(path) {
            Some(memo) if memo.length == length => memo.pulse.clone(),
            Some(memo) if memo.length < length => {
                let mut pulse = memo.pulse.clone();
                absorb_suffix(path, memo.length, &mut pulse)?;
                pulse
            }
            _ => scan_reverse(path, length)?,
        };
        let summary = RolloutSummary {
            preview: pulse.preview.clone(),
            running: pulse.running,
            waiting_for_input: pulse.input_call.is_some(),
            account: pulse.account.clone(),
        };
        let _prior = self.memo.insert(path.to_owned(), Memo { length, pulse });
        Ok(summary)
    }
}

fn absorb_suffix(path: &Path, offset: u64, pulse: &mut Pulse) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let _position = file.seek(SeekFrom::Start(offset))?;
    for line in BufReader::new(file).split(b'\n') {
        pulse.absorb(&line?);
    }
    Ok(())
}

fn scan_reverse(path: &Path, length: u64) -> std::io::Result<Pulse> {
    let mut file = File::open(path)?;
    let mut cursor = length;
    let mut suffix = Vec::new();
    let mut newest = Pulse::default();
    let mut found_work = false;
    let mut found_preview = false;
    let mut found_input_call = false;
    let mut found_account = false;
    let mut account_horizon_exhausted = false;
    let mut resolved_calls = HashSet::new();

    while cursor > 0
        && !(found_work && found_preview && (found_account || account_horizon_exhausted))
    {
        let start = cursor.saturating_sub(BLOCK as u64);
        let span = usize::try_from(cursor - start).unwrap_or(BLOCK);
        let mut bytes = vec![0; span];
        let _position = file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        bytes.extend_from_slice(&suffix);

        let first_break = memchr::memchr(b'\n', &bytes);
        let complete_from = if start == 0 {
            0
        } else {
            first_break.map_or(bytes.len(), |index| index + 1)
        };
        for line in bytes[complete_from..].split(|byte| *byte == b'\n').rev() {
            inspect_reverse(
                line,
                &mut newest,
                &mut found_work,
                &mut found_preview,
                &mut found_input_call,
                &mut found_account,
                &mut resolved_calls,
            );
            if found_work && found_preview && found_account {
                break;
            }
        }
        suffix = bytes[..complete_from.saturating_sub(1)].to_vec();
        cursor = start;
        account_horizon_exhausted = length.saturating_sub(start) >= ACCOUNT_HORIZON;
    }
    Ok(newest)
}

fn inspect_reverse(
    line: &[u8],
    newest: &mut Pulse,
    found_work: &mut bool,
    found_preview: &mut bool,
    found_input_call: &mut bool,
    found_account: &mut bool,
    resolved_calls: &mut HashSet<String>,
) {
    if memmem::find(line, CALL_OUTPUT).is_some() {
        if let Some(call) = call_id(line) {
            let _new = resolved_calls.insert(call.to_owned());
        }
        return;
    }
    if !*found_input_call && memmem::find(line, INPUT_REQUEST).is_some() {
        if let Some(call) = call_id(line)
            && !resolved_calls.contains(call)
        {
            newest.input_call = Some(call.to_owned());
        }
        *found_input_call = true;
        return;
    }
    if !interesting(line) {
        return;
    }
    let Ok(event) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if event.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    let payload = &event["payload"];
    match payload.get("type").and_then(Value::as_str) {
        Some("token_count") if !*found_account => {
            newest.account = payload
                .get("rate_limits")
                .and_then(quota)
                .map(AccountMark::quota);
            *found_account = true;
        }
        Some("task_started" | "turn_started") if !*found_work => {
            newest.running = true;
            *found_work = true;
        }
        Some("task_complete" | "turn_complete" | "turn_aborted") if !*found_work => {
            newest.running = false;
            *found_work = true;
            if !*found_preview
                && let Some(message) = payload.get("last_agent_message").and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                assign_preview(&mut newest.preview, message);
                *found_preview = true;
            }
        }
        Some("user_message" | "agent_message") if !*found_preview => {
            if let Some(message) = payload.get("message").and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                assign_preview(&mut newest.preview, message);
                *found_preview = true;
            }
        }
        _ => {}
    }
}

fn quota(value: &Value) -> Option<QuotaMark> {
    let primary = value.get("primary")?;
    Some(QuotaMark {
        limit: value.get("limit_id")?.as_str()?.to_owned(),
        window_minutes: primary.get("window_minutes")?.as_i64()?,
        resets_at: primary.get("resets_at")?.as_i64()?,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn fixture(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("fixture");
        for line in lines {
            writeln!(file, "{line}").expect("write fixture");
        }
        file
    }

    #[test]
    fn active_turn_is_running() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"forge it"}}"#,
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"active"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let summary = Rollouts::default().read(file.path()).expect("summarize");
        assert!(summary.running);
        assert_eq!(summary.preview, "forge it");
    }

    #[test]
    fn appended_completion_retires_a_turn_and_replaces_the_preview() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        assert!(rollouts.read(file.path()).expect("running").running);
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"slain\"}}\n",
        )
        .expect("append completion");
        let summary = rollouts.read(file.path()).expect("complete");
        assert!(!summary.running);
        assert_eq!(summary.preview, "slain");
    }

    #[test]
    fn an_idle_rollout_is_not_running() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"active"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        assert!(
            !Rollouts::default()
                .read(file.path())
                .expect("summarize")
                .running
        );
    }

    #[test]
    fn goal_events_do_not_own_running_state() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"paused"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        assert!(
            Rollouts::default()
                .read(file.path())
                .expect("summarize")
                .running
        );
    }

    #[test]
    fn unanswered_input_request_is_a_distinct_wait() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_7"}}"#,
        ]);
        let summary = Rollouts::default().read(file.path()).expect("summarize");
        assert!(summary.running);
        assert!(summary.waiting_for_input);
    }

    #[test]
    fn input_response_releases_the_wait_incrementally() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_7"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        assert!(
            rollouts
                .read(file.path())
                .expect("waiting")
                .waiting_for_input
        );
        file.write_all(
            b"{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call_7\"}}\n",
        )
        .expect("append response");
        assert!(
            !rollouts
                .read(file.path())
                .expect("released")
                .waiting_for_input
        );
    }

    #[test]
    fn answered_input_request_is_not_a_wait_on_cold_scan() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_7"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_7"}}"#,
        ]);
        assert!(
            !Rollouts::default()
                .read(file.path())
                .expect("answered")
                .waiting_for_input
        );
    }

    #[test]
    fn account_quota_survives_cold_and_incremental_scans() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"window_minutes":10080,"resets_at":1000000}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        let first = rollouts.read(file.path()).expect("cold quota");
        assert!(
            !first
                .account
                .expect("cold account")
                .rotated_to(&AccountMark::quota(QuotaMark {
                    limit: "codex".to_owned(),
                    window_minutes: 10_080,
                    resets_at: 1_000_000,
                }))
        );

        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"limit_id\":\"codex\",\"primary\":{\"window_minutes\":10080,\"resets_at\":2000000}}}}\n",
        )
        .expect("append quota");
        let second = rollouts.read(file.path()).expect("incremental quota");
        assert!(
            !second
                .account
                .expect("incremental account")
                .rotated_to(&AccountMark::quota(QuotaMark {
                    limit: "codex".to_owned(),
                    window_minutes: 10_080,
                    resets_at: 2_000_000,
                }))
        );
    }
}

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
const WIRE_PEER: &str = "wire-peer/";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TurnState {
    #[default]
    Unknown,
    Running,
    Done,
    Error,
}

#[derive(Clone, Debug, Default)]
struct Pulse {
    state: TurnState,
    input_call: Option<String>,
    delegated_turn: bool,
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
            Some("task_started" | "turn_started") => {
                self.state = TurnState::Running;
                self.input_call = None;
                self.delegated_turn = false;
            }
            Some(kind @ ("task_complete" | "turn_complete" | "turn_aborted")) => {
                self.state = completion_state(kind, payload);
                self.input_call = None;
                self.delegated_turn = false;
                if let Some(message) = payload.get("last_agent_message").and_then(Value::as_str) {
                    assign_preview(&mut self.preview, message);
                }
            }
            Some("user_message" | "agent_message" | "item_completed") => {
                if let Some(delegated) = delegated_user_message(payload) {
                    self.delegated_turn = delegated;
                }
                absorb_preview(&mut self.preview, payload);
            }
            Some(kind) if unknown_transition(kind) => {
                self.state = TurnState::Unknown;
                self.input_call = None;
                self.delegated_turn = false;
            }
            _ => {}
        }
    }
}

fn absorb_preview(slot: &mut String, payload: &Value) -> bool {
    match payload.get("type").and_then(Value::as_str) {
        Some("user_message" | "agent_message") => payload
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| assign_preview(slot, message)),
        Some("item_completed") => absorb_item_preview(slot, &payload["item"]),
        _ => false,
    }
}

fn absorb_item_preview(slot: &mut String, item: &Value) -> bool {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return false;
    };
    match item.get("type").and_then(Value::as_str) {
        Some("UserMessage") => {
            let message = content.iter().filter_map(text_content).collect::<String>();
            assign_preview(slot, &message)
        }
        Some("AgentMessage") => {
            let mut assigned = false;
            for message in content.iter().filter_map(text_content) {
                assigned |= assign_preview(slot, message);
            }
            assigned
        }
        _ => false,
    }
}

fn delegated_user_message(payload: &Value) -> Option<bool> {
    match payload.get("type").and_then(Value::as_str) {
        Some("user_message") => Some(false),
        Some("item_completed")
            if payload["item"].get("type").and_then(Value::as_str) == Some("UserMessage") =>
        {
            Some(
                payload["item"]
                    .get("client_id")
                    .and_then(Value::as_str)
                    .is_some_and(|client| client.starts_with(WIRE_PEER)),
            )
        }
        _ => None,
    }
}

fn text_content(content: &Value) -> Option<&str> {
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    content.get("text").and_then(Value::as_str)
}

fn assign_preview(slot: &mut String, message: &str) -> bool {
    let message = message.trim();
    if message.is_empty() {
        return false;
    }
    message.clone_into(slot);
    true
}

fn completion_state(kind: &str, payload: &Value) -> TurnState {
    if payload.get("error").is_some_and(|error| !error.is_null())
        || (kind == "turn_aborted"
            && payload.get("reason").and_then(Value::as_str) != Some("interrupted"))
    {
        TurnState::Error
    } else {
        TurnState::Done
    }
}

fn unknown_transition(kind: &str) -> bool {
    (kind.starts_with("task_") || kind.starts_with("turn_"))
        && !matches!(kind, "turn_diff" | "turn_moderation_metadata")
}

fn interesting(line: &[u8]) -> bool {
    [
        b"\"type\":\"task_".as_slice(),
        b"\"type\":\"turn_".as_slice(),
        b"\"type\":\"user_message\"".as_slice(),
        b"\"type\":\"agent_message\"".as_slice(),
        b"\"type\":\"item_completed\"".as_slice(),
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
    pub state: TurnState,
    pub waiting_for_input: bool,
    pub delegated_turn: bool,
    pub account: Option<AccountMark>,
}

impl RolloutSummary {
    /// Codex 0.147 creates the authoritative thread row and writer lock before
    /// materializing a rollout. Until the first turn, that is a lawful stopped
    /// session rather than a failed read.
    pub fn quiescent() -> Self {
        Self {
            preview: String::new(),
            state: TurnState::Done,
            waiting_for_input: false,
            delegated_turn: false,
            account: None,
        }
    }
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
            state: pulse.state,
            waiting_for_input: pulse.input_call.is_some(),
            delegated_turn: pulse.delegated_turn,
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

#[derive(Default)]
struct ReverseFrontier(u8);

impl ReverseFrontier {
    const ACCOUNT: u8 = 1 << 0;
    const DELEGATION: u8 = 1 << 1;
    const INPUT_CALL: u8 = 1 << 2;
    const PREVIEW: u8 = 1 << 3;
    const WORK: u8 = 1 << 4;

    const fn has(&self, finding: u8) -> bool {
        self.0 & finding != 0
    }

    fn mark(&mut self, finding: u8) {
        self.0 |= finding;
    }

    const fn resolved(&self, account_horizon_exhausted: bool) -> bool {
        self.has(Self::WORK)
            && self.has(Self::PREVIEW)
            && self.has(Self::DELEGATION)
            && (self.has(Self::ACCOUNT) || account_horizon_exhausted)
    }

    const fn fully_resolved(&self) -> bool {
        self.resolved(false)
    }
}

fn scan_reverse(path: &Path, length: u64) -> std::io::Result<Pulse> {
    let mut file = File::open(path)?;
    let mut cursor = length;
    let mut suffix = Vec::new();
    let mut newest = Pulse::default();
    let mut frontier = ReverseFrontier::default();
    let mut account_horizon_exhausted = false;
    let mut resolved_calls = HashSet::new();

    while cursor > 0 && !frontier.resolved(account_horizon_exhausted) {
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
            inspect_reverse(line, &mut newest, &mut frontier, &mut resolved_calls);
            if frontier.fully_resolved() {
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
    frontier: &mut ReverseFrontier,
    resolved_calls: &mut HashSet<String>,
) {
    if memmem::find(line, CALL_OUTPUT).is_some() {
        if let Some(call) = call_id(line) {
            let _new = resolved_calls.insert(call.to_owned());
        }
        return;
    }
    if !frontier.has(ReverseFrontier::INPUT_CALL) && memmem::find(line, INPUT_REQUEST).is_some() {
        if let Some(call) = call_id(line)
            && !resolved_calls.contains(call)
        {
            newest.input_call = Some(call.to_owned());
        }
        frontier.mark(ReverseFrontier::INPUT_CALL);
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
        Some("token_count") if !frontier.has(ReverseFrontier::ACCOUNT) => {
            newest.account = payload
                .get("rate_limits")
                .and_then(quota)
                .map(AccountMark::quota);
            frontier.mark(ReverseFrontier::ACCOUNT);
        }
        Some("task_started" | "turn_started") if !frontier.has(ReverseFrontier::WORK) => {
            newest.state = TurnState::Running;
            frontier.mark(ReverseFrontier::WORK | ReverseFrontier::INPUT_CALL);
        }
        Some(kind @ ("task_complete" | "turn_complete" | "turn_aborted"))
            if !frontier.has(ReverseFrontier::WORK) =>
        {
            newest.state = completion_state(kind, payload);
            frontier.mark(
                ReverseFrontier::WORK | ReverseFrontier::INPUT_CALL | ReverseFrontier::DELEGATION,
            );
            if !frontier.has(ReverseFrontier::PREVIEW)
                && let Some(message) = payload.get("last_agent_message").and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                assign_preview(&mut newest.preview, message);
                frontier.mark(ReverseFrontier::PREVIEW);
            }
        }
        Some("user_message" | "agent_message" | "item_completed") => {
            if !frontier.has(ReverseFrontier::DELEGATION)
                && let Some(delegated) = delegated_user_message(payload)
            {
                newest.delegated_turn = delegated;
                frontier.mark(ReverseFrontier::DELEGATION);
            }
            if !frontier.has(ReverseFrontier::PREVIEW)
                && absorb_preview(&mut newest.preview, payload)
            {
                frontier.mark(ReverseFrontier::PREVIEW);
            }
        }
        Some(kind) if !frontier.has(ReverseFrontier::WORK) && unknown_transition(kind) => {
            newest.state = TurnState::Unknown;
            frontier.mark(
                ReverseFrontier::WORK | ReverseFrontier::INPUT_CALL | ReverseFrontier::DELEGATION,
            );
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
        assert_eq!(summary.state, TurnState::Running);
        assert_eq!(summary.preview, "forge it");
    }

    #[test]
    fn legacy_and_paginated_user_messages_converge() {
        let legacy = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"forge it"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let paginated = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-1","turn_id":"turn-1","item":{"type":"UserMessage","id":"user-1","content":[{"type":"text","text":" forge ","text_elements":[]},{"type":"image","image_url":"ignored"},{"type":"text","text":"it ","text_elements":[]}]},"started_at_ms":1,"completed_at_ms":1}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);

        for file in [&legacy, &paginated] {
            let summary = Rollouts::default().read(file.path()).expect("summarize");
            assert_eq!(summary.state, TurnState::Running);
            assert_eq!(summary.preview, "forge it");
        }
    }

    #[test]
    fn paginated_agent_preview_survives_failed_completion() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-1","turn_id":"turn-1","item":{"type":"UserMessage","id":"user-1","content":[{"type":"text","text":"begin","text_elements":[]}]},"started_at_ms":1,"completed_at_ms":1}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        assert_eq!(
            rollouts.read(file.path()).expect("initial").preview,
            "begin"
        );
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\",\"item\":{\"type\":\"AgentMessage\",\"id\":\"agent-1\",\"content\":[{\"type\":\"text\",\"text\":\"progress\"},{\"type\":\"text\",\"text\":\"final fragment\"}]},\"started_at_ms\":2,\"completed_at_ms\":3}}\n",
        )
        .expect("append agent item");
        assert_eq!(
            rollouts.read(file.path()).expect("agent item").preview,
            "final fragment"
        );
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":null,\"error\":{\"message\":\"failed\"}}}\n",
        )
        .expect("append failed completion");

        let incremental = rollouts.read(file.path()).expect("incremental completion");
        assert_eq!(incremental.state, TurnState::Error);
        assert!(!incremental.waiting_for_input);
        assert_eq!(incremental.preview, "final fragment");
        let cold = Rollouts::default()
            .read(file.path())
            .expect("cold completion");
        assert_eq!(cold.state, TurnState::Error);
        assert!(!cold.waiting_for_input);
        assert_eq!(cold.preview, "final fragment");
    }

    #[test]
    fn delegated_provenance_survives_cold_preview_and_incremental_replacement() {
        let mut file = fixture(&[r#"{"type":"event_msg","payload":{"type":"task_started"}}"#]);
        let mut rollouts = Rollouts::default();
        assert!(
            !rollouts
                .read(file.path())
                .expect("turn before delegated message")
                .delegated_turn
        );
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"client_id\":\"wire-peer/post-1\",\"content\":[{\"type\":\"text\",\"text\":\"Wire advisory.\"}]}}}\n\
              {\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"content\":[{\"type\":\"text\",\"text\":\"delegated progress\"}]}}}\n",
        )
        .expect("append delegated turn");
        for summary in [
            rollouts
                .read(file.path())
                .expect("incremental delegated turn"),
            Rollouts::default()
                .read(file.path())
                .expect("cold delegated turn"),
        ] {
            assert_eq!(summary.state, TurnState::Running);
            assert!(summary.delegated_turn);
            assert_eq!(summary.preview, "delegated progress");
        }

        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n\
              {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n\
              {\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"content\":[{\"type\":\"text\",\"text\":\"human turn\"}]}}}\n",
        )
        .expect("append ordinary turn");
        for summary in [
            rollouts
                .read(file.path())
                .expect("incremental ordinary turn"),
            Rollouts::default()
                .read(file.path())
                .expect("cold ordinary turn"),
        ] {
            assert_eq!(summary.state, TurnState::Running);
            assert!(!summary.delegated_turn);
            assert_eq!(summary.preview, "human turn");
        }
    }

    #[test]
    fn every_completion_error_is_error_until_another_turn_starts() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"inspect it"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        let running = rollouts.read(file.path()).expect("running");
        assert_eq!(running.state, TurnState::Running);
        assert!(!running.waiting_for_input);

        file.write_all(
            br#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":null,"error":{"message":"A future upstream halt.","codex_error_info":"brand_new_halt"}}}
"#,
        )
        .expect("append unknown error code");
        for summary in [
            rollouts.read(file.path()).expect("incremental error"),
            Rollouts::default().read(file.path()).expect("cold error"),
        ] {
            assert_eq!(summary.state, TurnState::Error);
            assert!(!summary.waiting_for_input);
            assert_eq!(summary.preview, "inspect it");
        }

        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n")
            .expect("append next turn");
        for summary in [
            rollouts.read(file.path()).expect("incremental restart"),
            Rollouts::default().read(file.path()).expect("cold restart"),
        ] {
            assert_eq!(summary.state, TurnState::Running);
            assert!(!summary.waiting_for_input);
        }
    }

    #[test]
    fn unknown_task_transition_fails_closed() {
        let mut file = fixture(&[r#"{"type":"event_msg","payload":{"type":"task_started"}}"#]);
        let mut rollouts = Rollouts::default();
        assert_eq!(
            rollouts.read(file.path()).expect("running").state,
            TurnState::Running
        );
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_suspended_by_future_codex\"}}\n",
        )
        .expect("append unknown transition");
        for summary in [
            rollouts.read(file.path()).expect("incremental unknown"),
            Rollouts::default().read(file.path()).expect("cold unknown"),
        ] {
            assert_eq!(summary.state, TurnState::Unknown);
        }
    }

    #[test]
    fn only_explicit_user_interruption_is_a_clean_abort() {
        for (reason, state) in [
            ("interrupted", TurnState::Done),
            ("future_abort", TurnState::Error),
        ] {
            let line = format!(
                r#"{{"type":"event_msg","payload":{{"type":"turn_aborted","reason":"{reason}"}}}}"#
            );
            let file = fixture(&[&line]);
            assert_eq!(
                Rollouts::default().read(file.path()).expect("abort").state,
                state
            );
        }
    }

    #[test]
    fn completion_retires_an_unanswered_input_request_on_a_cold_scan() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_7"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}"#,
        ]);
        let summary = Rollouts::default().read(file.path()).expect("summarize");
        assert_eq!(summary.state, TurnState::Done);
        assert!(!summary.waiting_for_input);
    }

    #[test]
    fn appended_completion_retires_a_turn_and_replaces_the_preview() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        assert_eq!(
            rollouts.read(file.path()).expect("running").state,
            TurnState::Running
        );
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"slain\"}}\n",
        )
        .expect("append completion");
        let summary = rollouts.read(file.path()).expect("complete");
        assert_eq!(summary.state, TurnState::Done);
        assert_eq!(summary.preview, "slain");
    }

    #[test]
    fn an_idle_rollout_is_not_running() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"active"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        assert_eq!(
            Rollouts::default()
                .read(file.path())
                .expect("summarize")
                .state,
            TurnState::Done
        );
    }

    #[test]
    fn goal_events_do_not_own_running_state() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"paused"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        assert_eq!(
            Rollouts::default()
                .read(file.path())
                .expect("summarize")
                .state,
            TurnState::Running
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
        assert_eq!(summary.state, TurnState::Running);
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

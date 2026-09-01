use serde_json::Value;

pub(crate) enum CompletedMessage {
    User(String),
    Agent(String),
}

pub(crate) fn completed_message(item: &Value) -> Option<CompletedMessage> {
    // Codex 0.152 deliberately has asymmetric wire tags: UserInputContent is
    // `text`, while AgentMessageContent inherits the Rust variant name `Text`.
    let (block_type, separator, forge): (&str, &str, fn(String) -> CompletedMessage) =
        match item.get("type").and_then(Value::as_str) {
            Some("UserMessage") => ("text", "", CompletedMessage::User),
            Some("AgentMessage") => ("Text", "\n", CompletedMessage::Agent),
            _ => return None,
        };
    let message = item
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(block_type))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(separator);
    (!message.trim().is_empty()).then(|| forge(message))
}

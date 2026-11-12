//! The JSON lines `claude --output-format stream-json` prints, as Rust types.
//!
//! Only the fields this app uses are listed; serde ignores the rest, and any
//! line type we do not know becomes `Line::Other`. That keeps the app working
//! when the CLI adds new fields or message types.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    /// Housekeeping. The `init` subtype is first and names the exact model.
    System { subtype: String, model: Option<String> },
    /// Something the agent said or a tool it wants to call.
    Assistant {
        message: Message,
        /// Set to e.g. "authentication_failed" when the API call itself failed.
        error: Option<String>,
    },
    /// Tool results going back to the agent.
    User { message: Message },
    /// The last line: the final answer and what the session used.
    Result(ResultLine),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    /// Either plain text or a list of blocks, so it is parsed by hand in `blocks()`.
    #[serde(default)]
    content: Value,
}

impl Message {
    /// The content blocks we understand; anything else is skipped.
    pub fn blocks(&self) -> Vec<Block> {
        match &self.content {
            Value::Array(items) => {
                items.iter().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect()
            }
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        #[serde(default)]
        content: Value,
        #[serde(default)]
        is_error: Option<bool>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct ResultLine {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub is_error: bool,
    pub result: Option<String>,
    pub structured_output: Option<Value>,
    pub session_id: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
}

/// Parse one line of output. Returns `None` for blank or non-JSON lines.
pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// The text inside a tool result, whichever of its two shapes it has.
pub fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_line_gives_the_model() {
        let line =
            r#"{"type":"system","subtype":"init","cwd":"/tmp","model":"claude-opus-5-5","tools":["Read"]}"#;
        match parse_line(line) {
            Some(Line::System { subtype, model }) => {
                assert_eq!(subtype, "init");
                assert_eq!(model.as_deref(), Some("claude-opus-5-5"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn assistant_line_gives_text_and_tool_calls() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"hm","signature":"x"},
            {"type":"text","text":"Reading the file."},
            {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/a.py"}}]},
            "session_id":"s"}"#
            .replace('\n', "");
        let Some(Line::Assistant { message, error }) = parse_line(&line) else { panic!("not assistant") };
        assert!(error.is_none());
        let blocks = message.blocks();
        assert_eq!(blocks[0], Block::Other);
        assert_eq!(blocks[1], Block::Text { text: "Reading the file.".to_string() });
        assert!(matches!(&blocks[2], Block::ToolUse { name, .. } if name == "Read"));
    }

    #[test]
    fn user_line_gives_failed_tool_results() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Permission denied","is_error":true}]}}"#;
        let Some(Line::User { message }) = parse_line(line) else { panic!("not user") };
        match &message.blocks()[0] {
            Block::ToolResult { content, is_error } => {
                assert_eq!(*is_error, Some(true));
                assert_eq!(tool_result_text(content), "Permission denied");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn user_line_with_plain_text_has_no_blocks() {
        let line = r#"{"type":"user","message":{"role":"user","content":"hello"}}"#;
        let Some(Line::User { message }) = parse_line(line) else { panic!("not user") };
        assert!(message.blocks().is_empty());
    }

    #[test]
    fn result_line_captured_from_a_real_run() {
        // Trimmed copy of what the CLI printed for a failed sign-in.
        let line = r#"{"duration_api_ms":0,"session_id":"d80f","total_cost_usd":0,"usage":{"input_tokens":0},"terminal_reason":"api_error","is_error":true,"num_turns":1,"subtype":"success","result":"Authentication error","type":"result","duration_ms":84}"#;
        let Some(Line::Result(result)) = parse_line(line) else { panic!("not result") };
        assert!(result.is_error);
        assert_eq!(result.num_turns, Some(1));
        assert_eq!(result.session_id.as_deref(), Some("d80f"));
        assert!(result.structured_output.is_none());
    }

    #[test]
    fn unknown_and_broken_lines_do_not_fail() {
        assert!(matches!(parse_line(r#"{"type":"autocompact_state","value":{}}"#), Some(Line::Other)));
        assert!(parse_line("not json").is_none());
        assert!(parse_line("   ").is_none());
    }
}

use crate::kiro::model::requests::conversation::{ConversationState, Message};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::model::config::CompressionConfig;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompressionStats {
    pub whitespace_saved: usize,
    pub tool_result_saved: usize,
    pub tool_use_input_saved: usize,
    pub tool_definition_saved: usize,
}

impl CompressionStats {
    pub fn total_saved(&self) -> usize {
        self.whitespace_saved
            + self.tool_result_saved
            + self.tool_use_input_saved
            + self.tool_definition_saved
    }
}

pub fn compress(state: &mut ConversationState, config: &CompressionConfig) -> CompressionStats {
    let mut stats = CompressionStats::default();

    if !config.enabled {
        return stats;
    }

    if config.whitespace_compression {
        stats.whitespace_saved = compress_whitespace_pass(state);
    }

    if config.tool_result_max_chars > 0 {
        stats.tool_result_saved = compress_tool_results_pass(
            state,
            config.tool_result_max_chars,
            config.tool_result_head_lines,
            config.tool_result_tail_lines,
        );
    }

    if config.tool_use_input_max_chars > 0 {
        stats.tool_use_input_saved =
            compress_tool_use_inputs_pass(state, config.tool_use_input_max_chars);
    }

    stats
}

pub(crate) fn compress_kiro_request(
    request: &mut KiroRequest,
    config: &CompressionConfig,
) -> CompressionStats {
    if !config.enabled {
        return CompressionStats::default();
    }

    let mut stats = CompressionStats::default();
    if config.tool_definition_max_bytes > 0 {
        let tools = &mut request
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;
        if !tools.is_empty() {
            let before = super::tool_compression::estimate_tools_size(tools);
            *tools = super::tool_compression::compress_tools_if_needed(
                tools,
                config.tool_definition_max_bytes,
            );
            let after = super::tool_compression::estimate_tools_size(tools);
            stats.tool_definition_saved = before.saturating_sub(after);
        }
    }

    let body_stats = compress(&mut request.conversation_state, config);
    stats.whitespace_saved = body_stats.whitespace_saved;
    stats.tool_result_saved = body_stats.tool_result_saved;
    stats.tool_use_input_saved = body_stats.tool_use_input_saved;
    stats
}

fn compress_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut consecutive_empty = 0u32;

    for line in text.split('\n') {
        let trimmed_end = line.trim_end();
        if trimmed_end.is_empty() {
            consecutive_empty += 1;
            if consecutive_empty <= 1 && !result.is_empty() {
                result.push('\n');
            }
        } else {
            consecutive_empty = 0;
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(trimmed_end);
        }
    }

    result
}

fn compress_string_field(field: &mut String) -> usize {
    if field.trim().is_empty() {
        return 0;
    }

    let original_len = field.len();
    let compressed = compress_whitespace(field);
    if compressed.len() < original_len {
        let saved = original_len - compressed.len();
        *field = compressed;
        saved
    } else {
        0
    }
}

fn compress_whitespace_pass(state: &mut ConversationState) -> usize {
    let mut saved = 0usize;

    for msg in &mut state.history {
        match msg {
            Message::User(user_msg) => {
                saved += compress_string_field(&mut user_msg.user_input_message.content);
            }
            Message::Assistant(assistant_msg) => {
                saved +=
                    compress_string_field(&mut assistant_msg.assistant_response_message.content);
            }
        }
    }

    saved += compress_string_field(&mut state.current_message.user_input_message.content);
    saved
}

fn safe_char_truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn tail_by_chars(s: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }

    match s.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        Some((idx, _)) => &s[idx..],
        None => s,
    }
}

fn smart_truncate_by_lines(
    text: &str,
    max_chars: usize,
    head_lines: usize,
    tail_lines: usize,
) -> (String, usize) {
    let char_count = text.chars().count();
    if max_chars == 0 || char_count <= max_chars {
        return (text.to_string(), 0);
    }

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let sum_lines = head_lines.saturating_add(tail_lines);
    let result = if total_lines > sum_lines && sum_lines > 0 {
        let head_part = lines[..head_lines].join("\n");
        let tail_part = lines[total_lines - tail_lines..].join("\n");
        let omitted_lines = total_lines.saturating_sub(sum_lines);
        let omitted_chars =
            char_count.saturating_sub(head_part.chars().count() + tail_part.chars().count());
        let marked = format!(
            "{}\n... [{} lines omitted ({} chars)] ...\n{}",
            head_part, omitted_lines, omitted_chars, tail_part
        );
        if marked.chars().count() <= max_chars {
            marked
        } else {
            truncate_preserving_head_tail(text, max_chars)
        }
    } else {
        truncate_preserving_head_tail(text, max_chars)
    };

    let saved = text.len().saturating_sub(result.len());
    (result, saved)
}

fn truncate_preserving_head_tail(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if max_chars == 0 || char_count <= max_chars {
        return text.to_string();
    }

    let omitted = char_count.saturating_sub(max_chars);
    let full_marker = format!("\n... [{} chars omitted] ...\n", omitted);
    let marker = if full_marker.chars().count() < max_chars {
        full_marker
    } else {
        "\n...\n".to_string()
    };
    let marker_chars = marker.chars().count();
    if marker_chars >= max_chars {
        return safe_char_truncate(text, max_chars).to_string();
    }

    let remaining = max_chars - marker_chars;
    let head_chars = remaining / 2;
    let tail_chars = remaining - head_chars;
    let head = safe_char_truncate(text, head_chars);
    let tail = tail_by_chars(text, tail_chars);
    format!("{head}{marker}{tail}")
}

fn truncate_tool_result_content(
    content: &mut [serde_json::Map<String, serde_json::Value>],
    max_chars: usize,
    head_lines: usize,
    tail_lines: usize,
) -> usize {
    let mut saved = 0usize;

    for map in content.iter_mut() {
        if let Some(serde_json::Value::String(text)) = map.get_mut("text")
            && text.chars().count() > max_chars
        {
            let (truncated, current_saved) =
                smart_truncate_by_lines(text, max_chars, head_lines, tail_lines);
            saved += current_saved;
            *text = truncated;
        }
    }

    saved
}

fn compress_tool_results_pass(
    state: &mut ConversationState,
    max_chars: usize,
    head_lines: usize,
    tail_lines: usize,
) -> usize {
    let mut saved = 0usize;

    for msg in &mut state.history {
        if let Message::User(user_msg) = msg {
            for result in &mut user_msg
                .user_input_message
                .user_input_message_context
                .tool_results
            {
                saved += truncate_tool_result_content(
                    &mut result.content,
                    max_chars,
                    head_lines,
                    tail_lines,
                );
            }
        }
    }

    for result in &mut state
        .current_message
        .user_input_message
        .user_input_message_context
        .tool_results
    {
        saved +=
            truncate_tool_result_content(&mut result.content, max_chars, head_lines, tail_lines);
    }

    saved
}

fn truncate_json_value_strings(value: &mut serde_json::Value, max_chars: usize) -> usize {
    let mut saved = 0usize;

    match value {
        serde_json::Value::String(s) => {
            let original_char_count = s.chars().count();
            if original_char_count > max_chars {
                let original_len = s.len();
                let truncated = safe_char_truncate(s, max_chars).to_string();
                let omitted_chars = original_char_count.saturating_sub(max_chars);
                let with_marker = format!(
                    "{}...[truncated {} chars]",
                    truncated.as_str(),
                    omitted_chars
                );
                let new_value = if with_marker.len() < original_len {
                    with_marker
                } else {
                    truncated
                };
                saved += original_len.saturating_sub(new_value.len());
                *s = new_value;
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                saved += truncate_json_value_strings(value, max_chars);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                saved += truncate_json_value_strings(value, max_chars);
            }
        }
        _ => {}
    }

    saved
}

fn compress_tool_use_inputs_pass(state: &mut ConversationState, max_chars: usize) -> usize {
    let mut saved = 0usize;

    for msg in &mut state.history {
        if let Message::Assistant(assistant_msg) = msg
            && let Some(tool_uses) = assistant_msg.assistant_response_message.tool_uses.as_mut()
        {
            for tool_use in tool_uses {
                let serialized = serde_json::to_string(&tool_use.input).unwrap_or_default();
                if serialized.chars().count() > max_chars {
                    saved += truncate_json_value_strings(&mut tool_use.input, max_chars);
                }
            }
        }
    }

    saved
}

#[cfg(test)]
mod tests {
    use crate::kiro::model::requests::conversation::{
        AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
        HistoryUserMessage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
    };
    use crate::kiro::model::requests::kiro::KiroRequest;
    use crate::kiro::model::requests::tool::{InputSchema, Tool, ToolSpecification};
    use crate::kiro::model::requests::tool::{ToolResult, ToolUseEntry};
    use crate::model::config::CompressionConfig;

    #[test]
    fn low_risk_compression_compacts_whitespace_and_truncates_tool_payloads() {
        let mut assistant = AssistantMessage::new("using tool");
        assistant =
            assistant.with_tool_uses(vec![ToolUseEntry::new("toolu_1", "fs_write").with_input(
                serde_json::json!({
                    "path": "/tmp/large.txt",
                    "text": "a".repeat(1000)
                }),
            )]);

        let history_user = Message::User(HistoryUserMessage::new(
            "line 1  \n\n\n\nline 2",
            "claude-sonnet-4.5",
        ));
        let history_assistant = Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: assistant,
        });
        let current = UserInputMessage::new("current  \n\n\n\nmessage", "claude-sonnet-4.5")
            .with_context(
                UserInputMessageContext::new()
                    .with_tool_results(vec![ToolResult::success("toolu_1", "first\n".repeat(300))]),
            );

        let mut state = ConversationState::new("conversation")
            .with_current_message(CurrentMessage::new(current))
            .with_history(vec![history_user, history_assistant]);

        let stats = super::compress(
            &mut state,
            &CompressionConfig {
                tool_result_max_chars: 120,
                tool_result_head_lines: 2,
                tool_result_tail_lines: 2,
                tool_use_input_max_chars: 80,
                ..CompressionConfig::default()
            },
        );

        assert!(stats.whitespace_saved > 0);
        assert!(stats.tool_result_saved > 0);
        assert!(stats.tool_use_input_saved > 0);

        if let Message::User(user) = &state.history[0] {
            assert_eq!(user.user_input_message.content, "line 1\n\nline 2");
        } else {
            panic!("expected user history");
        }

        let result_text = state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results[0]
            .content[0]
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(result_text.chars().count() <= 120);
        assert!(result_text.contains("omitted"));

        if let Message::Assistant(assistant) = &state.history[1] {
            let tool_uses = assistant
                .assistant_response_message
                .tool_uses
                .as_ref()
                .unwrap();
            let text = tool_uses[0].input["text"].as_str().unwrap();
            assert!(text.chars().count() <= 80 + 32);
            assert!(text.contains("truncated") || text == "a".repeat(80));
        } else {
            panic!("expected assistant history");
        }
    }

    #[test]
    fn whitespace_compression_does_not_turn_blank_message_into_empty_string() {
        let mut state = ConversationState::new("conversation").with_current_message(
            CurrentMessage::new(UserInputMessage::new("   \n\n", "claude-sonnet-4.5")),
        );

        let stats = super::compress(
            &mut state,
            &CompressionConfig {
                tool_result_max_chars: 0,
                tool_use_input_max_chars: 0,
                tool_definition_max_bytes: 0,
                ..CompressionConfig::default()
            },
        );

        assert_eq!(stats.whitespace_saved, 0);
        assert_eq!(state.current_message.user_input_message.content, "   \n\n");
    }

    #[test]
    fn compression_does_not_modify_thinking_or_remove_history_turns() {
        let mut state = ConversationState::new("conversation")
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                "current",
                "claude-sonnet-4.5",
            )))
            .with_history(vec![
                Message::User(HistoryUserMessage {
                    user_input_message: UserMessage::new("old user", "claude-sonnet-4.5"),
                }),
                Message::Assistant(HistoryAssistantMessage::new(
                    "<thinking>private chain of thought</thinking>answer",
                )),
            ]);

        super::compress(&mut state, &CompressionConfig::default());

        assert_eq!(state.history.len(), 2);

        if let Message::Assistant(assistant) = &state.history[1] {
            assert!(
                assistant
                    .assistant_response_message
                    .content
                    .contains("<thinking>private chain of thought</thinking>")
            );
        } else {
            panic!("expected assistant history");
        }
    }

    #[test]
    fn tool_result_truncation_preserves_tail_when_line_marker_is_too_large() {
        let current = UserInputMessage::new("current", "claude-sonnet-4.5").with_context(
            UserInputMessageContext::new().with_tool_results(vec![ToolResult::success(
                "toolu_1",
                format!(
                    "{}\n{}\n{}",
                    "head".repeat(60),
                    "middle\n".repeat(200),
                    "tail".repeat(60)
                ),
            )]),
        );

        let mut state = ConversationState::new("conversation")
            .with_current_message(CurrentMessage::new(current));

        super::compress(
            &mut state,
            &CompressionConfig {
                tool_result_max_chars: 120,
                tool_result_head_lines: 80,
                tool_result_tail_lines: 40,
                ..CompressionConfig::default()
            },
        );

        let result_text = state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results[0]
            .content[0]
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(result_text.chars().count() <= 120);
        assert!(result_text.contains("head"));
        assert!(result_text.contains("tail"));
    }

    #[test]
    fn tool_result_line_truncation_does_not_panic_on_extreme_line_config() {
        let current = UserInputMessage::new("current", "claude-sonnet-4.5").with_context(
            UserInputMessageContext::new().with_tool_results(vec![ToolResult::success(
                "toolu_1",
                format!("{}\n{}", "head".repeat(50), "tail".repeat(50)),
            )]),
        );

        let mut state = ConversationState::new("conversation")
            .with_current_message(CurrentMessage::new(current));

        super::compress(
            &mut state,
            &CompressionConfig {
                tool_result_max_chars: 120,
                tool_result_head_lines: usize::MAX,
                tool_result_tail_lines: usize::MAX,
                ..CompressionConfig::default()
            },
        );

        let result_text = state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results[0]
            .content[0]
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(result_text.chars().count() <= 120);
    }

    #[test]
    fn compress_kiro_request_compresses_tool_definitions() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "long schema description"
                }
            },
            "required": ["path"]
        });
        let tools: Vec<_> = (0..20)
            .map(|idx| Tool {
                tool_specification: ToolSpecification {
                    name: format!("tool_{idx}"),
                    description: "x".repeat(2_000),
                    input_schema: InputSchema::from_json(schema.clone()),
                },
            })
            .collect();
        let current = UserInputMessage::new("current", "claude-sonnet-4.5")
            .with_context(UserInputMessageContext::new().with_tools(tools));
        let mut request = KiroRequest {
            conversation_state: ConversationState::new("conversation")
                .with_current_message(CurrentMessage::new(current)),
            profile_arn: None,
            additional_model_request_fields: None,
        };

        let stats = super::compress_kiro_request(
            &mut request,
            &CompressionConfig {
                tool_definition_max_bytes: 20 * 1024,
                ..CompressionConfig::default()
            },
        );

        assert!(stats.tool_definition_saved > 0);
        let path_schema = &request
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools[0]
            .tool_specification
            .input_schema
            .json["properties"]["path"];
        assert!(path_schema.get("description").is_none());
    }
}

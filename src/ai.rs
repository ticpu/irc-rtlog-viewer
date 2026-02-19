use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::AppState;
use crate::Channel;
use crate::server::{channel_dates, resolve_log_path, read_log_file};

pub enum SseEvent {
    ToolCall { name: String, input_summary: String },
    ToolResult { name: String, output_preview: String },
    Display(String),
    Done { url: String, output: String },
    Error(String),
}

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are an IRC log search assistant. Search IRC logs using tools and compile relevant excerpts into an output document.

Workflow:
1. Use display to tell the user what you're searching for
2. Use search to find relevant messages (use n first to gauge volume, then C for context)
3. Use copy to include relevant log lines in the output
4. Use output to add titles, separators, and factual summaries
5. Use done to save and finish -- you MUST always call done to produce a result

Rules:
- Only retrieve and summarize IRC log content
- Summaries must be grounded in the log data -- no speculation
- If the query is unrelated to IRC log search, call abort immediately
- NEVER respond with just text -- always produce an output document via done
- Format all output text as markdown (headings, lists, code blocks for log excerpts)
";

fn build_system_prompt(state: &AppState) -> String {
    let ai_config = state.config.ai.as_ref().unwrap();
    let base = ai_config.system_prompt.as_deref().unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let mut prompt = String::from(base);
    prompt.push_str("\nAvailable channels:\n");
    collect_channels(&state.channels, &mut prompt);
    prompt
}

fn collect_channels(node: &crate::ChannelNode, out: &mut String) {
    if let Some(channel) = &node.channel {
        if channel.name.starts_with('#') {
            let path = channel.path_segments.join("/");
            let dates = channel_dates(channel);
            if let (Some(first), Some(last)) = (dates.first(), dates.last()) {
                out.push_str(&format!(
                    "- {} ({} to {}, {} files)\n",
                    path,
                    first,
                    last,
                    dates.len()
                ));
            }
        }
    }
    for child in node.children.values() {
        collect_channels(child, out);
    }
}

fn build_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Grep-like search through IRC logs. Returns matching lines with line numbers.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Case-insensitive regex pattern to search for"
                    },
                    "channel": {
                        "type": "string",
                        "description": "Channel path (e.g. \"OFTC/#bcachefs-dev\")"
                    },
                    "date": {
                        "type": "string",
                        "description": "Specific date YYYY-MM-DD to search"
                    },
                    "from_date": {
                        "type": "string",
                        "description": "Start of date range YYYY-MM-DD (inclusive)"
                    },
                    "to_date": {
                        "type": "string",
                        "description": "End of date range YYYY-MM-DD (inclusive)"
                    },
                    "order": {
                        "type": "string",
                        "enum": ["newest", "oldest"],
                        "description": "Search order: newest-first (default) or oldest-first"
                    },
                    "A": {
                        "type": "integer",
                        "description": "Lines of context after each match"
                    },
                    "B": {
                        "type": "integer",
                        "description": "Lines of context before each match"
                    },
                    "C": {
                        "type": "integer",
                        "description": "Lines of context before and after each match"
                    },
                    "n": {
                        "type": "boolean",
                        "description": "Count-only mode: return match count per date instead of lines"
                    },
                    "c": {
                        "type": "integer",
                        "description": "Max number of matching lines to return (default 50)"
                    }
                },
                "required": ["pattern", "channel"]
            }
        }),
        json!({
            "name": "copy",
            "description": "Copy specific line ranges from a log file into the output buffer.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Channel path"
                    },
                    "date": {
                        "type": "string",
                        "description": "Date YYYY-MM-DD"
                    },
                    "lines": {
                        "type": "string",
                        "description": "Line spec: e.g. \"1,5,10,20-30,300-320\""
                    }
                },
                "required": ["channel", "date", "lines"]
            }
        }),
        json!({
            "name": "output",
            "description": "Append text to the output buffer (titles, separators, summaries).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to append"
                    },
                    "clear": {
                        "type": "boolean",
                        "description": "Clear the buffer before appending"
                    }
                },
                "required": ["text"]
            }
        }),
        json!({
            "name": "done",
            "description": "Save the output buffer to a file and finish the session.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Title for the output file (used to generate filename slug)"
                    }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "display",
            "description": "Show a progress message to the user in real-time.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Message to display"
                    }
                },
                "required": ["text"]
            }
        }),
        json!({
            "name": "abort",
            "description": "Cancel the session (use when the query is unrelated to IRC log search).",
            "input_schema": {
                "type": "object",
                "properties": {},
            }
        }),
    ]
}

fn validate_channel<'a>(
    channel_path: &str,
    channels: &'a crate::ChannelNode,
) -> Result<&'a Channel, String> {
    let segments: Vec<&str> = channel_path.split('/').collect();
    let mut node = channels;
    for seg in &segments {
        match node.children.get(*seg) {
            Some(child) => node = child,
            None => return Err(format!("unknown channel: {channel_path}")),
        }
    }
    match &node.channel {
        Some(ch) if ch.name.starts_with('#') => Ok(ch),
        Some(_) => Err(format!("channel not accessible: {channel_path}")),
        None => Err(format!("not a channel: {channel_path}")),
    }
}

fn execute_search(input: &Value, state: &AppState) -> String {
    let pattern = match input["pattern"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return "error: pattern is required".into(),
    };
    let channel_path = match input["channel"].as_str() {
        Some(c) => c,
        None => return "error: channel is required".into(),
    };
    let channel = match validate_channel(channel_path, &state.channels) {
        Ok(c) => c,
        Err(e) => return e,
    };

    let re = match regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
    {
        Ok(r) => r,
        Err(e) => return format!("invalid regex: {e}"),
    };

    let count_only = input["n"].as_bool().unwrap_or(false);
    let max_results = input["c"].as_u64().unwrap_or(50) as usize;
    let context_after = input["C"].as_u64().unwrap_or(0).max(input["A"].as_u64().unwrap_or(0)) as usize;
    let context_before = input["C"].as_u64().unwrap_or(0).max(input["B"].as_u64().unwrap_or(0)) as usize;

    let specific_date = input["date"].as_str().filter(|d| d.len() == 10);
    let from_date = input["from_date"].as_str().filter(|d| d.len() == 10);
    let to_date = input["to_date"].as_str().filter(|d| d.len() == 10);
    let oldest_first = input["order"].as_str() == Some("oldest");

    let dates: Vec<String> = if let Some(date) = specific_date {
        vec![date.to_string()]
    } else {
        let mut d = channel_dates(channel);
        if let Some(from) = from_date {
            d.retain(|date| date.as_str() >= from);
        }
        if let Some(to) = to_date {
            d.retain(|date| date.as_str() <= to);
        }
        if !oldest_first {
            d.reverse();
        }
        d
    };

    let mut out = String::new();
    let mut total_matches = 0;
    let mut dates_scanned = 0;

    for date in &dates {
        if dates_scanned >= 365 {
            out.push_str("\n[stopped: 365 dates scanned]\n");
            break;
        }
        dates_scanned += 1;

        let Some((path, _)) = resolve_log_path(channel, date) else { continue };
        let Ok(content) = read_log_file(&path) else { continue };
        let all_lines: Vec<&str> = content.lines().collect();

        let mut date_matches = 0;
        let mut emitted_lines: BTreeSet<usize> = BTreeSet::new();

        for (i, line) in all_lines.iter().enumerate() {
            if re.is_match(line) {
                date_matches += 1;
                if !count_only {
                    let start = i.saturating_sub(context_before);
                    let end = (i + context_after + 1).min(all_lines.len());
                    for j in start..end {
                        emitted_lines.insert(j);
                    }
                }
            }
        }

        if date_matches == 0 {
            continue;
        }

        if count_only {
            out.push_str(&format!("{date}: {date_matches} matches\n"));
            total_matches += date_matches;
            continue;
        }

        out.push_str(&format!("--- {channel_path} {date} ({date_matches} matches) ---\n"));
        let mut prev_line: Option<usize> = None;
        for j in &emitted_lines {
            if let Some(p) = prev_line {
                if *j > p + 1 {
                    out.push_str("--\n");
                }
            }
            out.push_str(&format!("{:>5}: {}\n", j + 1, all_lines[*j]));
            prev_line = Some(*j);
        }
        total_matches += date_matches;

        if total_matches >= max_results {
            out.push_str(&format!("\n[stopped: {max_results} match limit reached]\n"));
            break;
        }

        if out.len() > 8000 {
            out.push_str("\n[stopped: output size limit]\n");
            break;
        }
    }

    if total_matches == 0 {
        format!("no matches for \"{pattern}\" in {channel_path}")
    } else if count_only {
        format!("{out}total: {total_matches} matches across {dates_scanned} dates scanned")
    } else {
        out
    }
}

fn execute_copy(input: &Value, state: &AppState, output_buf: &mut String) -> String {
    let channel_path = match input["channel"].as_str() {
        Some(c) => c,
        None => return "error: channel is required".into(),
    };
    let date = match input["date"].as_str() {
        Some(d) if d.len() == 10 => d,
        _ => return "error: date (YYYY-MM-DD) is required".into(),
    };
    let lines_spec = match input["lines"].as_str() {
        Some(l) => l,
        None => return "error: lines spec is required".into(),
    };

    let channel = match validate_channel(channel_path, &state.channels) {
        Ok(c) => c,
        Err(e) => return e,
    };

    let line_nums = match parse_line_spec(lines_spec) {
        Ok(nums) => nums,
        Err(e) => return e,
    };

    if line_nums.len() > 500 {
        return "error: max 500 lines per copy".into();
    }

    let Some((path, _)) = resolve_log_path(channel, date) else {
        return format!("no log for {date} in {channel_path}");
    };
    let Ok(content) = read_log_file(&path) else {
        return format!("error reading log for {date}");
    };

    let all_lines: Vec<&str> = content.lines().collect();
    output_buf.push_str(&format!("--- {channel_path} {date} ---\n"));
    let mut copied = 0;
    for n in &line_nums {
        if *n == 0 || *n > all_lines.len() {
            continue;
        }
        output_buf.push_str(all_lines[*n - 1]);
        output_buf.push('\n');
        copied += 1;
    }

    if output_buf.len() > 100_000 {
        output_buf.truncate(100_000);
        return format!("copied {copied} lines (output buffer truncated to 100KB)");
    }

    format!("copied {copied} lines")
}

fn execute_output(input: &Value, output_buf: &mut String) -> String {
    let text = input["text"].as_str().unwrap_or("");
    if input["clear"].as_bool().unwrap_or(false) {
        output_buf.clear();
    }
    output_buf.push_str(text);
    output_buf.push('\n');

    if output_buf.len() > 100_000 {
        output_buf.truncate(100_000);
        return "appended (output buffer truncated to 100KB)".into();
    }

    "ok".into()
}

fn execute_done(
    input: &Value,
    state: &AppState,
    output_buf: &str,
    tx: &mpsc::UnboundedSender<SseEvent>,
) -> String {
    let title = match input["title"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return "error: title is required".into(),
    };

    let ai_config = state.config.ai.as_ref().unwrap();
    let slug = slugify(title);
    let filename = format!("{slug}.md");
    let path = ai_config.output_dir.join(&filename);

    if let Err(e) = std::fs::write(&path, output_buf) {
        eprintln!("ai: failed to write {}: {e}", path.display());
        return format!("error writing file: {e}");
    }

    let base_path = &state.config.base_path;
    let url = format!("{base_path}/ask/output/{slug}.html");

    let _ = tx.send(SseEvent::Done {
        url: url.clone(),
        output: output_buf.to_string(),
    });

    format!("saved: {url}")
}

fn execute_display(input: &Value, tx: &mpsc::UnboundedSender<SseEvent>) -> String {
    let text = input["text"].as_str().unwrap_or("");
    let _ = tx.send(SseEvent::Display(text.to_string()));
    "ok".into()
}

fn execute_abort(tx: &mpsc::UnboundedSender<SseEvent>) -> String {
    let _ = tx.send(SseEvent::Error(
        "no relevant results found".into(),
    ));
    "aborted".into()
}

pub async fn run_ai_session(
    query: String,
    _channel: Channel,
    state: Arc<AppState>,
    tx: mpsc::UnboundedSender<SseEvent>,
) {
    let ai_config = match &state.config.ai {
        Some(c) => c,
        None => {
            let _ = tx.send(SseEvent::Error("AI not configured".into()));
            return;
        }
    };
    let client = match &state.reqwest_client {
        Some(c) => c.clone(),
        None => {
            let _ = tx.send(SseEvent::Error("HTTP client not available".into()));
            return;
        }
    };

    let api_url = "https://api.anthropic.com/v1/messages";

    let system_prompt = build_system_prompt(&state);
    let tools = build_tool_definitions();
    let model = ai_config.model.clone();
    let api_key = ai_config.api_key.clone();

    let mut messages: Vec<Value> = vec![json!({
        "role": "user",
        "content": query,
    })];

    // Put cache_control on the last tool so tools+system prefix can be cached.
    // Haiku 4.5 requires 4096 tokens minimum; the prefix will only be cached
    // once the conversation grows past that threshold.
    let mut tools = tools;
    if let Some(last_tool) = tools.last_mut() {
        last_tool["cache_control"] = json!({"type": "ephemeral"});
    }

    let max_tool_calls = ai_config.max_tool_calls;

    let mut output_buf = String::new();

    for _iteration in 0..max_tool_calls {
        let msg_json = serde_json::to_string(&messages).unwrap_or_default();
        if msg_json.len() > 150_000 {
            let _ = tx.send(SseEvent::Error("context limit reached".into()));
            break;
        }

        // Move cache_control to the last message so the growing conversation
        // prefix gets cached. Clear old markers first (max 4 breakpoints allowed).
        for msg in messages.iter_mut() {
            if let Some(content) = msg["content"].as_array_mut() {
                for block in content.iter_mut() {
                    if let Some(obj) = block.as_object_mut() {
                        obj.remove("cache_control");
                    }
                }
            }
        }
        if let Some(last_msg) = messages.last_mut() {
            if let Some(content) = last_msg["content"].as_array_mut() {
                if let Some(last_block) = content.last_mut() {
                    last_block["cache_control"] = json!({"type": "ephemeral"});
                }
            } else if last_msg["content"].is_string() {
                let text = last_msg["content"].as_str().unwrap().to_string();
                last_msg["content"] = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": {"type": "ephemeral"},
                }]);
            }
        }

        let body = json!({
            "model": model,
            "max_tokens": 4096,
            "system": [{
                "type": "text",
                "text": system_prompt,
            }],
            "messages": messages,
            "tools": tools,
        });

        let resp = match client
            .post(api_url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ai: API request failed: {e}");
                let _ = tx.send(SseEvent::Error(format!("API request failed: {e}")));
                break;
            }
        };

        let status = resp.status();
        let resp_text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("ai: API read failed: {e}");
                let _ = tx.send(SseEvent::Error(format!("API read failed: {e}")));
                break;
            }
        };

        if !status.is_success() {
            eprintln!("ai: API error {status}: {resp_text}");
            let _ = tx.send(SseEvent::Error(format!("API error {status}: {resp_text}")));
            break;
        }

        let resp_json: Value = match serde_json::from_str(&resp_text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ai: invalid API response: {e}");
                let _ = tx.send(SseEvent::Error(format!("invalid API response: {e}")));
                break;
            }
        };

        if let Some(usage) = resp_json.get("usage") {
            let input = usage["input_tokens"].as_u64().unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let output = usage["output_tokens"].as_u64().unwrap_or(0);
            eprintln!(
                "ai: tokens: input={input} cache_create={cache_create} cache_read={cache_read} output={output}"
            );
        }

        let stop_reason = resp_json["stop_reason"].as_str().unwrap_or("");
        let content = resp_json["content"].as_array();

        if stop_reason == "end_turn" {
            let mut text_content = String::new();
            if let Some(blocks) = content {
                for block in blocks {
                    if block["type"].as_str() == Some("text") {
                        if let Some(text) = block["text"].as_str() {
                            text_content.push_str(text);
                        }
                    }
                }
            }

            if !text_content.trim().is_empty() {
                output_buf.push_str(&text_content);
                output_buf.push('\n');
            }

            if !output_buf.trim().is_empty() {
                let done_input = json!({"title": query});
                execute_done(&done_input, &state, &output_buf, &tx);
            } else {
                let _ = tx.send(SseEvent::Error("no results found".into()));
            }
            break;
        }

        if stop_reason == "tool_use" {
            let Some(blocks) = content else { break };
            let blocks: Vec<Value> = blocks.clone();

            messages.push(json!({
                "role": "assistant",
                "content": blocks,
            }));

            let mut tool_results: Vec<Value> = Vec::new();
            let mut should_stop = false;

            for block in &blocks {
                if block["type"].as_str() != Some("tool_use") {
                    continue;
                }

                let tool_name = block["name"].as_str().unwrap_or("");
                let tool_id = block["id"].as_str().unwrap_or("");
                let tool_input = &block["input"];

                let silent = matches!(tool_name, "display" | "abort");

                if !silent {
                    let input_summary = summarize_input(tool_name, tool_input);
                    let _ = tx.send(SseEvent::ToolCall {
                        name: tool_name.to_string(),
                        input_summary,
                    });
                }

                let result = match tool_name {
                    "search" => execute_search(tool_input, &state),
                    "copy" => execute_copy(tool_input, &state, &mut output_buf),
                    "output" => execute_output(tool_input, &mut output_buf),
                    "done" => {
                        should_stop = true;
                        execute_done(tool_input, &state, &output_buf, &tx)
                    }
                    "display" => execute_display(tool_input, &tx),
                    "abort" => {
                        should_stop = true;
                        execute_abort(&tx)
                    }
                    _ => format!("unknown tool: {tool_name}"),
                };

                if !silent {
                    let _ = tx.send(SseEvent::ToolResult {
                        name: tool_name.to_string(),
                        output_preview: result.clone(),
                    });
                }

                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": result,
                }));
            }

            messages.push(json!({
                "role": "user",
                "content": tool_results,
            }));

            if should_stop {
                break;
            }
        } else {
            let _ = tx.send(SseEvent::Error(format!(
                "unexpected stop_reason: {stop_reason}"
            )));
            break;
        }
    }
}

fn summarize_input(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "search" => {
            let pattern = input["pattern"].as_str().unwrap_or("?");
            let channel = input["channel"].as_str().unwrap_or("?");
            let mut s = format!("pattern={pattern:?}, channel={channel}");
            if let Some(date) = input["date"].as_str() {
                s.push_str(&format!(", date={date}"));
            }
            if let Some(from) = input["from_date"].as_str() {
                s.push_str(&format!(", from={from}"));
            }
            if let Some(to) = input["to_date"].as_str() {
                s.push_str(&format!(", to={to}"));
            }
            if let Some(order) = input["order"].as_str() {
                s.push_str(&format!(", order={order}"));
            }
            if input["n"].as_bool().unwrap_or(false) {
                s.push_str(", count-only");
            }
            if let Some(c) = input["C"].as_u64() {
                s.push_str(&format!(", C={c}"));
            }
            s
        }
        "copy" => {
            let channel = input["channel"].as_str().unwrap_or("?");
            let date = input["date"].as_str().unwrap_or("?");
            let lines = input["lines"].as_str().unwrap_or("?");
            format!("{channel} {date} lines={lines}")
        }
        "output" => {
            let text = input["text"].as_str().unwrap_or("");
            let preview = if text.len() > 60 { &text[..60] } else { text };
            format!("{preview:?}")
        }
        "done" => {
            let title = input["title"].as_str().unwrap_or("?");
            format!("title={title:?}")
        }
        "display" => {
            let text = input["text"].as_str().unwrap_or("");
            let preview = if text.len() > 80 { &text[..80] } else { text };
            preview.to_string()
        }
        "abort" => String::new(),
        _ => format!("{input}"),
    }
}

pub fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    for c in title.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' {
            slug.push(c);
        } else {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.len() > 120 {
        slug[..120].trim_end_matches('-').to_string()
    } else {
        slug
    }
}

pub fn parse_line_spec(spec: &str) -> Result<BTreeSet<usize>, String> {
    if spec.is_empty() {
        return Err("empty line spec".into());
    }
    if !spec.bytes().all(|b| b.is_ascii_digit() || b == b',' || b == b'-') {
        return Err("invalid line spec: only digits, commas, and hyphens allowed".into());
    }

    let mut result = BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start_s, end_s)) = part.split_once('-') {
            let start: usize = start_s
                .parse()
                .map_err(|_| format!("invalid number: {start_s}"))?;
            let end: usize = end_s
                .parse()
                .map_err(|_| format!("invalid number: {end_s}"))?;
            if start == 0 || end == 0 {
                return Err("line numbers must be >= 1".into());
            }
            if end < start {
                return Err(format!("invalid range: {start}-{end}"));
            }
            if end - start > 500 {
                return Err("range too large (max 500 lines)".into());
            }
            for n in start..=end {
                result.insert(n);
            }
        } else {
            let n: usize = part
                .parse()
                .map_err(|_| format!("invalid number: {part}"))?;
            if n == 0 {
                return Err("line numbers must be >= 1".into());
            }
            result.insert(n);
        }
    }

    if result.len() > 500 {
        return Err("too many lines (max 500)".into());
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn test_slugify_date_prefix() {
        assert_eq!(
            slugify("2025-05-04--irc-bcache---promote-target"),
            "2025-05-04--irc-bcache---promote-target"
        );
    }

    #[test]
    fn test_slugify_special_chars() {
        assert_eq!(slugify("foo/bar: baz!"), "foo-bar--baz");
    }

    #[test]
    fn test_slugify_trim_hyphens() {
        assert_eq!(slugify("---hello---"), "hello");
    }

    #[test]
    fn test_slugify_truncate() {
        let long = "a".repeat(200);
        let slug = slugify(&long);
        assert!(slug.len() <= 120);
    }

    #[test]
    fn test_parse_line_spec_single() {
        let result = parse_line_spec("5").unwrap();
        assert_eq!(result, BTreeSet::from([5]));
    }

    #[test]
    fn test_parse_line_spec_list() {
        let result = parse_line_spec("1,5,10").unwrap();
        assert_eq!(result, BTreeSet::from([1, 5, 10]));
    }

    #[test]
    fn test_parse_line_spec_range() {
        let result = parse_line_spec("20-25").unwrap();
        assert_eq!(result, BTreeSet::from([20, 21, 22, 23, 24, 25]));
    }

    #[test]
    fn test_parse_line_spec_mixed() {
        let result = parse_line_spec("1,5,20-22").unwrap();
        assert_eq!(result, BTreeSet::from([1, 5, 20, 21, 22]));
    }

    #[test]
    fn test_parse_line_spec_zero() {
        assert!(parse_line_spec("0").is_err());
    }

    #[test]
    fn test_parse_line_spec_invalid() {
        assert!(parse_line_spec("abc").is_err());
    }

    #[test]
    fn test_parse_line_spec_empty() {
        assert!(parse_line_spec("").is_err());
    }

    #[test]
    fn test_parse_line_spec_reversed_range() {
        assert!(parse_line_spec("30-20").is_err());
    }
}

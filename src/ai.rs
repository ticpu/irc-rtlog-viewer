use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::AppState;
use crate::Channel;
use crate::parser::{LogFormat, parse_line};
use crate::server::{channel_dates, read_log_file, resolve_log_path, today_date};

pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// Room for adaptive thinking, which counts toward it, plus one tool call or answer.
const MAX_RESPONSE_TOKENS: u32 = 16_000;
/// Haiku 5.5's default, sent explicitly so a model change does not shift it.
const EFFORT: &str = "medium";
/// Old tool results are cleared server-side once a prompt passes this; the newest
/// KEEP_TOOL_USES stay, since `copy` works from their line numbers.
const CLEAR_TOOL_RESULTS_AT: u64 = 60_000;
const KEEP_TOOL_USES: u64 = 4;
/// Clearing invalidates the cache, so a pass must free at least this many tokens.
const CLEAR_AT_LEAST: u64 = 10_000;
/// A session whose prompt still passes this stops and keeps what it found;
/// Haiku 5.5 bills prompts over 100K tokens at five times the rate.
const MAX_PROMPT_TOKENS: u64 = 90_000;

/// Who an ask session bills its Anthropic requests to.
pub struct AskClient {
    /// The ask_id cookie value.
    pub id: String,
    /// The client address, see `Limits::client_addr`.
    pub addr: std::net::IpAddr,
}

pub enum SseEvent {
    ToolCall {
        name: String,
        input_summary: String,
    },
    ToolResult {
        name: String,
        output_preview: String,
    },
    Display(String),
    Done {
        url: String,
        output: String,
    },
    Error(String),
}

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are an IRC log search assistant. Search IRC logs using tools and compile relevant excerpts into an output document.

Workflow:
1. Use display to tell the user what you're searching for
2. Find the messages: activity shows which days had discussion, read goes through a day in order, search finds messages by regex across days (use n first to gauge volume, then C for context)
3. Use copy to include relevant log lines in the output
4. Use output to add titles, separators, and factual summaries
5. Use done to save and finish -- you MUST always call done to produce a result

Rules:
- The user message names the channel to search by default and gives today's date; search other channels only when the question calls for it
- Joins, parts, quits, nick changes and server notices are hidden unless you ask for events; they are not discussion, so leave them out of summaries unless the question is about them
- Only retrieve and summarize IRC log content
- Summaries must be grounded in the log data -- no speculation
- If the query is unrelated to IRC log search, call abort immediately
- NEVER respond with just text -- always produce an output document via done
- Format all output text as markdown (headings, lists, code blocks for log excerpts)
";

fn build_system_prompt(state: &AppState) -> String {
    let ai_config = state
        .config
        .ai
        .as_ref()
        .unwrap();
    let base = ai_config
        .system_prompt
        .as_deref()
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let mut prompt = String::from(base);
    prompt.push_str("\nAvailable channels:\n");
    collect_channels(&state.channels, &mut prompt);
    prompt
}

fn collect_channels(node: &crate::ChannelNode, out: &mut String) {
    if let Some(channel) = &node.channel {
        let path = channel
            .path_segments
            .join("/");
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
    for child in node
        .children
        .values()
    {
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
                    "events": {
                        "type": "boolean",
                        "description": "Include joins, parts, quits, nick changes and server notices (default false: only messages and actions are searched)"
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
            "name": "activity",
            "description": "Message count per day for a channel, newest day first. Days with logs but no messages (only joins, quits and notices) are counted in the summary line, not listed. Use it to find when a channel was active before reading or searching.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Channel path (e.g. \"OFTC/#bcachefs-dev\")"
                    },
                    "from_date": {
                        "type": "string",
                        "description": "Start of date range YYYY-MM-DD (inclusive)"
                    },
                    "to_date": {
                        "type": "string",
                        "description": "End of date range YYYY-MM-DD (inclusive)"
                    }
                },
                "required": ["channel"]
            }
        }),
        json!({
            "name": "read",
            "description": "Read one day of a channel's log in order, messages only, with line numbers for copy. Long days come in pages: the result ends with the from_line to continue from, or with the end of the day.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Channel path (e.g. \"OFTC/#bcachefs-dev\")"
                    },
                    "date": {
                        "type": "string",
                        "description": "Date YYYY-MM-DD"
                    },
                    "from_line": {
                        "type": "integer",
                        "description": "Line number to start from (default 1)"
                    },
                    "events": {
                        "type": "boolean",
                        "description": "Include joins, parts, quits, nick changes and server notices (default false)"
                    }
                },
                "required": ["channel", "date"]
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
    let segments: Vec<&str> = channel_path
        .split('/')
        .collect();
    let mut node = channels;
    for seg in &segments {
        match node
            .children
            .get(*seg)
        {
            Some(child) => node = child,
            None => return Err(format!("unknown channel: {channel_path}")),
        }
    }
    match &node.channel {
        Some(ch) => Ok(ch),
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

    let count_only = input["n"]
        .as_bool()
        .unwrap_or(false);
    let include_events = input["events"]
        .as_bool()
        .unwrap_or(false);
    let max_results = input["c"]
        .as_u64()
        .unwrap_or(50) as usize;
    let context_after = input["C"]
        .as_u64()
        .unwrap_or(0)
        .max(
            input["A"]
                .as_u64()
                .unwrap_or(0),
        ) as usize;
    let context_before = input["C"]
        .as_u64()
        .unwrap_or(0)
        .max(
            input["B"]
                .as_u64()
                .unwrap_or(0),
        ) as usize;

    let dates = match input["date"]
        .as_str()
        .filter(|d| d.len() == 10)
    {
        Some(date) => vec![date.to_string()],
        None => dates_in_range(channel, input),
    };

    let mut out = String::new();
    let mut total_matches = 0;
    let mut days_with_logs = 0;

    'dates: for date in &dates {
        if days_with_logs >= MAX_DAYS_SCANNED {
            out.push_str(&format!("\n[stopped: {MAX_DAYS_SCANNED} days scanned]\n"));
            break;
        }
        let Some((all_lines, format)) = day_lines(channel, date) else {
            continue;
        };
        days_with_logs += 1;

        let mut date_matches = 0;
        let mut emitted_lines: BTreeSet<usize> = BTreeSet::new();

        for (i, line) in all_lines
            .iter()
            .enumerate()
        {
            if is_shown(line, format, include_events) && re.is_match(line) {
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

        total_matches += date_matches;
        out.push_str(&format!(
            "--- {channel_path} {date} ({date_matches} matches) ---\n"
        ));
        let mut prev_line: Option<usize> = None;
        for j in &emitted_lines {
            if !is_shown(&all_lines[*j], format, include_events) {
                continue;
            }
            if let Some(p) = prev_line {
                if *j > p + 1 {
                    out.push_str("--\n");
                }
            }
            let quoted = format!("{:>5}: {}\n", j + 1, quote_line(&all_lines[*j]));
            if out.len() + quoted.len() > MAX_SEARCH_OUTPUT {
                out.push_str("\n[stopped: output size limit]\n");
                break 'dates;
            }
            out.push_str(&quoted);
            prev_line = Some(*j);
        }

        if total_matches >= max_results {
            out.push_str(&format!("\n[stopped: {max_results} match limit reached]\n"));
            break;
        }
    }

    if total_matches == 0 {
        format!(
            "no messages matching \"{pattern}\" in {channel_path} ({days_with_logs} days with \
             logs searched)"
        )
    } else if count_only {
        format!("{out}total: {total_matches} matches across {days_with_logs} days with logs")
    } else {
        out
    }
}

/// Days with logs, newest first unless `order` is "oldest", within the
/// optional `from_date` and `to_date` of a tool input.
fn dates_in_range(channel: &Channel, input: &Value) -> Vec<String> {
    let mut dates = channel_dates(channel);
    if let Some(from) = input["from_date"].as_str() {
        dates.retain(|date| date.as_str() >= from);
    }
    if let Some(to) = input["to_date"].as_str() {
        dates.retain(|date| date.as_str() <= to);
    }
    if input["order"].as_str() != Some("oldest") {
        dates.reverse();
    }
    dates
}

/// One day's lines with control characters stripped, or None when the day has
/// no log. Line N is index N-1: search, read and copy share this numbering.
fn day_lines(channel: &Channel, date: &str) -> Option<(Vec<String>, LogFormat)> {
    let (path, format) = resolve_log_path(channel, date)?;
    match read_log_file(&path) {
        Ok(content) => Some((
            content
                .lines()
                .map(strip_controls)
                .collect(),
            format,
        )),
        Err(e) => {
            eprintln!("ai: cannot read {}: {e}", path.display());
            None
        }
    }
}

/// Whether a line is shown to the model: not blank, and not a join, part, quit
/// or other notice unless events were asked for.
fn is_shown(line: &str, format: LogFormat, include_events: bool) -> bool {
    !line
        .trim()
        .is_empty()
        && (include_events || !parse_line(line, format).is_some_and(|parsed| parsed.is_event()))
}

fn execute_read(input: &Value, state: &AppState) -> String {
    let Some(channel_path) = input["channel"].as_str() else {
        return "error: channel is required".into();
    };
    let channel = match validate_channel(channel_path, &state.channels) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let Some(date) = input["date"].as_str() else {
        return "error: date (YYYY-MM-DD) is required".into();
    };
    let from_line = input["from_line"]
        .as_u64()
        .unwrap_or(1)
        .max(1) as usize;
    let include_events = input["events"]
        .as_bool()
        .unwrap_or(false);
    let Some((lines, format)) = day_lines(channel, date) else {
        return format!("no log for {date} in {channel_path}");
    };

    let mut out = format!("--- {channel_path} {date} ---\n");
    let mut shown = 0;
    for (i, line) in lines
        .iter()
        .enumerate()
        .skip(from_line - 1)
    {
        if !is_shown(line, format, include_events) {
            continue;
        }
        let quoted = format!("{:>5}: {}\n", i + 1, quote_line(line));
        if out.len() + quoted.len() > MAX_SEARCH_OUTPUT {
            out.push_str(&format!("[more: read again with from_line={}]\n", i + 1));
            return out;
        }
        out.push_str(&quoted);
        shown += 1;
    }
    if shown == 0 {
        return format!(
            "no messages in {channel_path} on {date} from line {from_line}; the log has {} \
             lines, none of them messages",
            lines.len()
        );
    }
    out.push_str(&format!("[end of {date}, {} lines]\n", lines.len()));
    out
}

fn execute_activity(input: &Value, state: &AppState) -> String {
    let Some(channel_path) = input["channel"].as_str() else {
        return "error: channel is required".into();
    };
    let channel = match validate_channel(channel_path, &state.channels) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut out = String::new();
    let mut days_with_logs = 0;
    let mut days_with_messages = 0;
    for date in dates_in_range(channel, input)
        .iter()
        .take(MAX_DAYS_SCANNED)
    {
        let Some((lines, format)) = day_lines(channel, date) else {
            continue;
        };
        days_with_logs += 1;
        let messages = lines
            .iter()
            .filter(|line| is_shown(line, format, false))
            .count();
        if messages == 0 {
            continue;
        }
        days_with_messages += 1;
        let row = format!("{date}: {messages} messages\n");
        if out.len() + row.len() > MAX_SEARCH_OUTPUT {
            out.push_str("[stopped: output size limit, narrow the date range]\n");
            break;
        }
        out.push_str(&row);
    }
    format!("{out}{days_with_logs} days with logs, {days_with_messages} of them with messages")
}

/// Most days with logs one search or activity call goes through.
const MAX_DAYS_SCANNED: usize = 365;
/// Bound on one search result, checked before each line is added.
const MAX_SEARCH_OUTPUT: usize = 8000;
/// Longest log line a search result quotes; the rest is elided with a count.
const MAX_QUOTED_LINE: usize = 500;

/// A log line without control characters other than tab. JSON escapes each as
/// six bytes, and a crash can leave a log ending in tens of KB of NULs.
fn strip_controls(line: &str) -> String {
    line.chars()
        .filter(|c| *c == '\t' || !c.is_control())
        .collect()
}

/// `line` cut to MAX_QUOTED_LINE characters, saying how many were left out.
fn quote_line(line: &str) -> String {
    let total = line
        .chars()
        .count();
    if total <= MAX_QUOTED_LINE {
        return line.to_owned();
    }
    let kept: String = line
        .chars()
        .take(MAX_QUOTED_LINE)
        .collect();
    format!("{kept} [+{} chars]", total - MAX_QUOTED_LINE)
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

    let Some((all_lines, _)) = day_lines(channel, date) else {
        return format!("no log for {date} in {channel_path}");
    };
    output_buf.push_str(&format!("--- {channel_path} {date} ---\n"));
    let mut copied = 0;
    for n in &line_nums {
        if *n == 0 || *n > all_lines.len() {
            continue;
        }
        output_buf.push_str(&all_lines[*n - 1]);
        output_buf.push('\n');
        copied += 1;
    }

    if output_buf.len() > 100_000 {
        output_buf.truncate(output_buf.floor_char_boundary(100_000));
        return format!("copied {copied} lines (output buffer truncated to 100KB)");
    }

    format!("copied {copied} lines")
}

fn execute_output(input: &Value, output_buf: &mut String) -> String {
    let text = input["text"]
        .as_str()
        .unwrap_or("");
    if input["clear"]
        .as_bool()
        .unwrap_or(false)
    {
        output_buf.clear();
    }
    output_buf.push_str(text);
    output_buf.push('\n');

    if output_buf.len() > 100_000 {
        output_buf.truncate(output_buf.floor_char_boundary(100_000));
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

    let ai_config = state
        .config
        .ai
        .as_ref()
        .unwrap();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let slug = slugify(title);
    let stem = match write_new_output(&ai_config.output_dir, &format!("{ts}-{slug}"), output_buf) {
        Ok(stem) => stem,
        Err(e) => {
            eprintln!(
                "ai: failed to write {ts}-{slug}.md in {}: {e}",
                ai_config
                    .output_dir
                    .display()
            );
            return format!("error writing file: {e}");
        }
    };

    let base_path = &state
        .config
        .base_path;
    let url = format!("{base_path}/ask/output/{stem}.html");

    let _ = tx.send(SseEvent::Done {
        url: url.clone(),
        output: output_buf.to_string(),
    });

    format!("saved: {url}")
}

/// Writes `{stem}.md`, or `{stem}-2.md` and up when another session already took
/// the name, and returns the stem used.
fn write_new_output(dir: &std::path::Path, stem: &str, content: &str) -> std::io::Result<String> {
    for n in 1..=100 {
        let candidate = if n == 1 {
            stem.to_owned()
        } else {
            format!("{stem}-{n}")
        };
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(format!("{candidate}.md")));
        match file {
            Ok(mut file) => {
                file.write_all(content.as_bytes())?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("{stem}.md and 99 numbered variants exist"),
    ))
}

fn execute_display(input: &Value, tx: &mpsc::UnboundedSender<SseEvent>) -> String {
    let text = input["text"]
        .as_str()
        .unwrap_or("");
    let _ = tx.send(SseEvent::Display(text.to_string()));
    "ok".into()
}

fn execute_abort(tx: &mpsc::UnboundedSender<SseEvent>) -> String {
    let _ = tx.send(SseEvent::Error("no relevant results found".into()));
    "aborted".into()
}

pub async fn run_ai_session(
    query: String,
    channel: Channel,
    state: Arc<AppState>,
    billed_to: AskClient,
    tx: mpsc::UnboundedSender<SseEvent>,
) {
    let ai_config = match &state
        .config
        .ai
    {
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

    #[cfg(not(test))]
    let api_url = ANTHROPIC_MESSAGES_URL;
    #[cfg(test)]
    let api_url = state
        .api_url
        .as_str();

    let system_prompt = build_system_prompt(&state);
    let tools = build_tool_definitions();
    let model = ai_config
        .model
        .clone();
    let api_key = ai_config
        .api_key
        .clone();

    // Scope goes in the user message, not the system prompt: system_prompt is
    // user-overridable, and the tools+system prefix is cached.
    let channel_path = channel
        .path_segments
        .join("/");
    // Newlines stripped so a query cannot forge its own scope line below.
    let query = query.replace(['\n', '\r'], " ");
    let mut messages: Vec<Value> = vec![json!({
        "role": "user",
        "content": format!(
            "Today's date (UTC): {today}\n\
             Default channel to search: {channel_path}\n\
             Search other channels only if the question below names them.\n\n\
             Question:\n{query}",
            today = today_date(),
        ),
    })];

    // The tools+system prefix is the same for every session; the top-level
    // cache_control below covers the growing conversation.
    let mut tools = tools;
    if let Some(last_tool) = tools.last_mut() {
        last_tool["cache_control"] = json!({"type": "ephemeral"});
    }

    let max_tool_calls = ai_config.max_tool_calls;

    let mut output_buf = String::new();
    // Prompt size the API reported for the last request, cached parts included.
    let mut prompt_tokens = 0;

    for iteration in 0..max_tool_calls {
        // The permit is held for the whole session, so a client that disconnects
        // would otherwise keep billing and block the next request.
        if tx.is_closed() {
            eprintln!("ask: client disconnected, abandoning session");
            return;
        }

        if prompt_tokens > MAX_PROMPT_TOKENS {
            let reason = format!("the conversation passed {MAX_PROMPT_TOKENS} tokens");
            finish_early(&reason, &query, &state, &output_buf, &tx);
            return;
        }

        // serve_ask_stream charged the first request before starting the session.
        if iteration > 0 {
            if let Err(refusal) = state
                .limits
                .charge_ask(&billed_to.id, billed_to.addr)
            {
                let _ = tx.send(SseEvent::Error(refusal.to_string()));
                return;
            }
        }

        // Earlier messages are never edited: Haiku 5.5 rejects replayed thinking
        // blocks once anything before them changes.
        let body = json!({
            "model": model,
            "max_tokens": MAX_RESPONSE_TOKENS,
            "output_config": {"effort": EFFORT},
            "cache_control": {"type": "ephemeral"},
            "context_management": {"edits": [{
                "type": "clear_tool_uses_20250919",
                "trigger": {"type": "input_tokens", "value": CLEAR_TOOL_RESULTS_AT},
                "keep": {"type": "tool_uses", "value": KEEP_TOOL_USES},
                "clear_at_least": {"type": "input_tokens", "value": CLEAR_AT_LEAST},
            }]},
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
            .header("anthropic-beta", "context-management-2025-06-27")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ai: API request failed: {e}");
                let _ = tx.send(SseEvent::Error(format!("API request failed: {e}")));
                return;
            }
        };

        let status = resp.status();
        let resp_text = match resp
            .text()
            .await
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("ai: API read failed: {e}");
                let _ = tx.send(SseEvent::Error(format!("API read failed: {e}")));
                return;
            }
        };

        if !status.is_success() {
            eprintln!("ai: API error {status}: {resp_text}");
            let _ = tx.send(SseEvent::Error(format!("API error {status}")));
            return;
        }

        let resp_json: Value = match serde_json::from_str(&resp_text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ai: invalid API response: {e}");
                let _ = tx.send(SseEvent::Error(format!("invalid API response: {e}")));
                return;
            }
        };

        if let Some(usage) = resp_json.get("usage") {
            let input = usage["input_tokens"]
                .as_u64()
                .unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"]
                .as_u64()
                .unwrap_or(0);
            let output = usage["output_tokens"]
                .as_u64()
                .unwrap_or(0);
            eprintln!(
                "ai: tokens: input={input} cache_create={cache_create} cache_read={cache_read} output={output}"
            );
            prompt_tokens = input + cache_create + cache_read;
        }
        for edit in resp_json["context_management"]["applied_edits"]
            .as_array()
            .into_iter()
            .flatten()
        {
            eprintln!(
                "ai: context editing cleared {} tool uses, {} input tokens",
                edit["cleared_tool_uses"], edit["cleared_input_tokens"]
            );
        }

        let stop_reason = resp_json["stop_reason"]
            .as_str()
            .unwrap_or("");
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

            if !text_content
                .trim()
                .is_empty()
            {
                output_buf.push_str(&text_content);
                output_buf.push('\n');
            }

            if !output_buf
                .trim()
                .is_empty()
            {
                let done_input = json!({"title": query});
                execute_done(&done_input, &state, &output_buf, &tx);
            } else {
                let _ = tx.send(SseEvent::Error("no results found".into()));
            }
            return;
        }

        if stop_reason == "tool_use" {
            let Some(blocks) = content else { return };
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

                let tool_name = block["name"]
                    .as_str()
                    .unwrap_or("");
                let tool_id = block["id"]
                    .as_str()
                    .unwrap_or("");
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
                    "search" | "read" | "activity" => {
                        let state = Arc::clone(&state);
                        let input = tool_input.clone();
                        let name = tool_name.to_owned();
                        tokio::task::spawn_blocking(move || match name.as_str() {
                            "search" => execute_search(&input, &state),
                            "read" => execute_read(&input, &state),
                            _ => execute_activity(&input, &state),
                        })
                        .await
                        .unwrap_or_else(|e| format!("error: {tool_name} failed: {e}"))
                    }
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
                return;
            }
        } else {
            let msg = match stop_reason {
                "refusal" => format!(
                    "the model declined this question ({})",
                    resp_json["stop_details"]["category"]
                        .as_str()
                        .unwrap_or("no category given")
                ),
                "max_tokens" => format!("the model's reply exceeded {MAX_RESPONSE_TOKENS} tokens"),
                other => format!("unexpected stop_reason: {other}"),
            };
            eprintln!("ai: session stopped: {msg}");
            let _ = tx.send(SseEvent::Error(msg));
            return;
        }
    }
    let reason = format!("stopped after {max_tool_calls} API requests");
    finish_early(&reason, &query, &state, &output_buf, &tx);
}

/// Ends a session that stopped before calling done: saves what the output buffer
/// holds and says why it is partial, or reports `reason` alone.
fn finish_early(
    reason: &str,
    query: &str,
    state: &AppState,
    output_buf: &str,
    tx: &mpsc::UnboundedSender<SseEvent>,
) {
    eprintln!("ai: session stopped early: {reason}");
    if output_buf
        .trim()
        .is_empty()
    {
        let _ = tx.send(SseEvent::Error(reason.to_owned()));
        return;
    }
    let _ = tx.send(SseEvent::Display(format!(
        "{reason}; saving what was found so far"
    )));
    execute_done(&json!({"title": query}), state, output_buf, tx);
}

fn summarize_input(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "search" => {
            let pattern = input["pattern"]
                .as_str()
                .unwrap_or("?");
            let channel = input["channel"]
                .as_str()
                .unwrap_or("?");
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
            if input["n"]
                .as_bool()
                .unwrap_or(false)
            {
                s.push_str(", count-only");
            }
            if let Some(c) = input["C"].as_u64() {
                s.push_str(&format!(", C={c}"));
            }
            if input["events"]
                .as_bool()
                .unwrap_or(false)
            {
                s.push_str(", with events");
            }
            s
        }
        "activity" => {
            let channel = input["channel"]
                .as_str()
                .unwrap_or("?");
            let from = input["from_date"]
                .as_str()
                .unwrap_or("first log");
            let to = input["to_date"]
                .as_str()
                .unwrap_or("last log");
            format!("{channel} {from} to {to}")
        }
        "read" => {
            let channel = input["channel"]
                .as_str()
                .unwrap_or("?");
            let date = input["date"]
                .as_str()
                .unwrap_or("?");
            let from = input["from_line"]
                .as_u64()
                .unwrap_or(1);
            let mut s = format!("{channel} {date} from line {from}");
            if input["events"]
                .as_bool()
                .unwrap_or(false)
            {
                s.push_str(", with events");
            }
            s
        }
        "copy" => {
            let channel = input["channel"]
                .as_str()
                .unwrap_or("?");
            let date = input["date"]
                .as_str()
                .unwrap_or("?");
            let lines = input["lines"]
                .as_str()
                .unwrap_or("?");
            format!("{channel} {date} lines={lines}")
        }
        "output" => {
            let text = input["text"]
                .as_str()
                .unwrap_or("");
            let preview = &text[..text.floor_char_boundary(60)];
            format!("{preview:?}")
        }
        "done" => {
            let title = input["title"]
                .as_str()
                .unwrap_or("?");
            format!("title={title:?}")
        }
        "display" => {
            let text = input["text"]
                .as_str()
                .unwrap_or("");
            let preview = &text[..text.floor_char_boundary(80)];
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
    let slug = slug
        .trim_matches('-')
        .to_string();
    if slug.len() > 120 {
        slug[..120]
            .trim_end_matches('-')
            .to_string()
    } else {
        slug
    }
}

pub fn parse_line_spec(spec: &str) -> Result<BTreeSet<usize>, String> {
    if spec.is_empty() {
        return Err("empty line spec".into());
    }
    if !spec
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b',' || b == b'-')
    {
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
    use crate::parser::LogFormat;
    use crate::test_support::{add_channel, app_state, tempdir};
    use crate::{ChannelDir, ChannelNode};

    /// A `#c` channel whose directory has a `secret1.log` sibling.
    fn state_with_secret_sibling(root: &std::path::Path) -> AppState {
        let dir = root.join("#c");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("2026-01-01.log"),
            "2026-01-01T00:00:00Z <a> SECRET? no\n",
        )
        .unwrap();
        std::fs::write(
            root.join("secret1.log"),
            "2026-01-01T00:00:00Z <a> SECRET\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        add_channel(
            &mut channels,
            &["#c"],
            vec![ChannelDir {
                path: dir,
                format: LogFormat::Iso8601,
            }],
        );
        app_state(channels, "logs_dirs: []")
    }

    #[test]
    fn search_results_stay_small_and_free_of_control_characters() {
        let root = tempdir();
        let dir = root
            .path()
            .join("#c");
        std::fs::create_dir(&dir).unwrap();
        let long = "x".repeat(2000);
        let nuls = "\0".repeat(70_000);
        std::fs::write(
            dir.join("2026-01-01.log"),
            format!("2026-01-01T00:00:00Z <a> {long}\n2026-01-01T00:00:01Z <b> hi\n{nuls}"),
        )
        .unwrap();
        let many: String = (0..2000)
            .map(|i| format!("2026-01-02T00:00:00Z <a> line {i}\n"))
            .collect();
        std::fs::write(dir.join("2026-01-02.log"), many).unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: dir,
            format: LogFormat::Iso8601,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let state = app_state(channels, "logs_dirs: []");

        let one_day = execute_search(
            &json!({"pattern": ".", "channel": "#c", "date": "2026-01-01"}),
            &state,
        );
        assert!(!one_day.contains('\0'), "NULs reached the model");
        assert!(one_day.contains("[+1525 chars]"), "{one_day}");
        assert!(one_day.contains("(2 matches)"), "{one_day}");

        let big_day = execute_search(
            &json!({"pattern": "line", "channel": "#c", "date": "2026-01-02", "c": 5000}),
            &state,
        );
        assert!(
            big_day.len() <= MAX_SEARCH_OUTPUT + 40,
            "{} bytes",
            big_day.len()
        );
        assert!(
            big_day.ends_with("[stopped: output size limit]\n"),
            "{big_day}"
        );
    }

    /// A ZNC-format `#c` channel: a mixed day, a day of joins only, a long day.
    fn state_with_days(root: &std::path::Path) -> AppState {
        let dir = root.join("#c");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("2026-01-01.log"),
            "[00:00:01] *** Joins: a (~a@host)\n\
             [00:00:02] *** Server sets mode: +nt \n\
             [00:00:03] <a> first message\n\
             [00:00:04] *** Quits: b (~b@host) (bye)\n\
             [00:00:05] * a waves\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("2026-01-02.log"),
            "[00:00:01] *** Joins: a (~a@host)\n[00:00:02] *** Quits: a (~a@host) (bye)\n",
        )
        .unwrap();
        let long: String = (1..=400)
            .map(|i| format!("[00:00:00] <a> message number {i}\n"))
            .collect();
        std::fs::write(dir.join("2026-01-03.log"), long).unwrap();
        let mut channels = ChannelNode::default();
        add_channel(
            &mut channels,
            &["#c"],
            vec![ChannelDir {
                path: dir,
                format: LogFormat::Znc,
            }],
        );
        app_state(channels, "logs_dirs: []")
    }

    #[test]
    fn activity_lists_days_with_messages_only() {
        let root = tempdir();
        let state = state_with_days(root.path());
        let out = execute_activity(&json!({"channel": "#c"}), &state);
        assert_eq!(
            out,
            "2026-01-03: 400 messages\n2026-01-01: 2 messages\n3 days with logs, 2 of them with \
             messages"
        );
    }

    #[test]
    fn read_shows_messages_in_order_and_pages_long_days() {
        let root = tempdir();
        let state = state_with_days(root.path());

        let mixed = execute_read(&json!({"channel": "#c", "date": "2026-01-01"}), &state);
        assert_eq!(
            mixed,
            "--- #c 2026-01-01 ---\n    3: [00:00:03] <a> first message\n    5: [00:00:05] * a \
             waves\n[end of 2026-01-01, 5 lines]\n"
        );
        let mut buf = String::new();
        execute_copy(
            &json!({"channel": "#c", "date": "2026-01-01", "lines": "3"}),
            &state,
            &mut buf,
        );
        assert!(buf.ends_with("[00:00:03] <a> first message\n"), "{buf}");

        let quiet = execute_read(&json!({"channel": "#c", "date": "2026-01-02"}), &state);
        assert!(quiet.contains("2 lines, none of them messages"), "{quiet}");
        let with_events = execute_read(
            &json!({"channel": "#c", "date": "2026-01-02", "events": true}),
            &state,
        );
        assert!(with_events.contains("*** Joins: a"), "{with_events}");

        let mut from = 1;
        let mut seen = 0;
        loop {
            let page = execute_read(
                &json!({"channel": "#c", "date": "2026-01-03", "from_line": from}),
                &state,
            );
            assert!(page.len() <= MAX_SEARCH_OUTPUT + 60, "{} bytes", page.len());
            seen += page
                .matches("message number")
                .count();
            if page.ends_with("[end of 2026-01-03, 400 lines]\n") {
                break;
            }
            let next = page
                .rsplit("from_line=")
                .next()
                .unwrap()
                .trim_end_matches("]\n")
                .parse::<u64>()
                .unwrap();
            assert!(
                page.contains(&format!("message number {}\n", next - 1)),
                "page ends before {next}"
            );
            from = next;
        }
        assert_eq!(seen, 400, "every line is read exactly once across pages");
    }

    #[test]
    fn search_without_matches_says_how_much_it_searched() {
        let root = tempdir();
        let state = state_with_days(root.path());
        let out = execute_search(&json!({"pattern": "nothing", "channel": "#c"}), &state);
        assert_eq!(
            out,
            "no messages matching \"nothing\" in #c (3 days with logs searched)"
        );
    }

    #[test]
    fn same_named_outputs_do_not_overwrite_each_other() {
        let dir = tempdir();
        let first = write_new_output(dir.path(), "1-t", "one").unwrap();
        let second = write_new_output(dir.path(), "1-t", "two").unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("1-t", "1-t-2"));
        let read = |stem: &str| {
            std::fs::read_to_string(
                dir.path()
                    .join(format!("{stem}.md")),
            )
            .unwrap()
        };
        assert_eq!((read(&first), read(&second)), ("one".into(), "two".into()));
    }

    #[test]
    fn tools_cannot_read_outside_the_channel_dir() {
        let root = tempdir();
        let state = state_with_secret_sibling(root.path());
        let date = "../secret1";
        let control = execute_search(
            &json!({"pattern": "SECRET", "channel": "#c", "date": "2026-01-01"}),
            &state,
        );
        assert!(control.contains("SECRET? no"), "{control}");

        let out = execute_search(
            &json!({"pattern": "SECRET", "channel": "#c", "date": date}),
            &state,
        );
        assert!(!out.contains("<a> SECRET\n"), "{out}");

        let mut buf = String::new();
        execute_copy(
            &json!({"channel": "#c", "date": date, "lines": "1"}),
            &state,
            &mut buf,
        );
        assert!(!buf.contains("<a> SECRET\n"), "{buf}");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let text = format!("{}é", "a".repeat(59));
        assert!(summarize_input("output", &json!({"text": text})).starts_with("\"aaa"));
        let text = format!("{}é", "a".repeat(79));
        assert_eq!(
            summarize_input("display", &json!({"text": text})),
            "a".repeat(79)
        );

        let text = format!("{}é", "a".repeat(99_999));
        let mut buf = String::new();
        execute_output(&json!({"text": text}), &mut buf);
        assert_eq!(buf, "a".repeat(99_999));
    }

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

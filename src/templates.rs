use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::parser::{LineKind, LogLine};
use crate::search::SearchResults;
use crate::{Channel, ChannelNode};

fn nick_hue(nick: &str) -> u16 {
    let mut hash: u32 = 5381;
    for b in nick.bytes() {
        hash = hash
            .wrapping_mul(33)
            .wrapping_add(u32::from(b));
    }
    (hash % 360) as u16
}

fn nick_color_style(nick: &str) -> String {
    format!("color:hsl({},70%,65%)", nick_hue(nick))
}

pub fn linkify(text: &str) -> Markup {
    let mut result = String::new();
    let mut last = 0;

    for (i, _) in text.match_indices("http") {
        if i < last {
            continue;
        }
        let rest = &text[i..];
        let scheme_end = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            continue;
        };

        if scheme_end >= rest.len() {
            continue;
        }

        let url_end = rest[scheme_end..]
            .find(|c: char| c.is_whitespace())
            .map(|p| scheme_end + p)
            .unwrap_or(rest.len());

        let url = &rest[..url_end];
        let before = &text[last..i];

        result.push_str(&maud::html! { (before) }.into_string());
        result.push_str(
            &maud::html! { a href=(url) target="_blank" rel="noopener" { (url) } }.into_string(),
        );
        last = i + url_end;
    }

    if last < text.len() {
        result.push_str(&maud::html! { (&text[last..]) }.into_string());
    }

    PreEscaped(result)
}

pub fn render_line(line: &LogLine) -> Markup {
    let anchor = line
        .time
        .to_anchor();
    let ts = line
        .time
        .to_hms();
    let class = if line.is_event() {
        "line event"
    } else {
        "line"
    };

    html! {
        div class=(class) id=(&anchor) {
            a.ts href=(format!("#{anchor}")) { (ts) }
            " "
            @match &line.kind {
                LineKind::Message { nick, text } => {
                    span.nick style=(nick_color_style(nick)) { "<" (nick) ">" }
                    " "
                    span.msg { (linkify(text)) }
                },
                LineKind::Action { nick, text } => {
                    span.action {
                        "* "
                        span.nick style=(nick_color_style(nick)) { (nick) }
                        " "
                        (linkify(text))
                    }
                },
                LineKind::Join { nick, userhost } => {
                    span.ev {
                        "→ "
                        span.nick style=(nick_color_style(nick)) { (nick) }
                        " (" (userhost) ") joined"
                    }
                },
                LineKind::Quit { nick, userhost, reason } => {
                    span.ev {
                        "← "
                        span.nick style=(nick_color_style(nick)) { (nick) }
                        " (" (userhost) ") quit"
                        @if !reason.is_empty() {
                            " (" (reason) ")"
                        }
                    }
                },
                LineKind::Part { nick, userhost, reason } => {
                    span.ev {
                        "← "
                        span.nick style=(nick_color_style(nick)) { (nick) }
                        " (" (userhost) ") left"
                        @if !reason.is_empty() {
                            " (" (reason) ")"
                        }
                    }
                },
                LineKind::NickChange { old_nick, new_nick } => {
                    span.ev {
                        span.nick style=(nick_color_style(old_nick)) { (old_nick) }
                        " → "
                        span.nick style=(nick_color_style(new_nick)) { (new_nick) }
                    }
                },
            }
        }
    }
}

fn render_channel_tree(node: &ChannelNode, parent_path: &str, base_path: &str) -> Markup {
    html! {
        ul {
            @for (name, child) in &node.children {
                li {
                    @let child_path = if parent_path.is_empty() {
                        name.clone()
                    } else {
                        format!("{parent_path}/{name}")
                    };
                    @if let Some(channel) = &child.channel {
                        a href=(format!("{base_path}/{}/today", channel.url_path())) { (&channel.name) }
                    } @else {
                        span.tree-label { (name) }
                    }
                    @if !child.children.is_empty() {
                        (render_channel_tree(child, &child_path, base_path))
                    }
                }
            }
        }
    }
}

pub fn page(title: &str, tree: &ChannelNode, base_path: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href=(format!("{base_path}/static/style.css"));
                script src=(format!("{base_path}/static/app.js")) defer {}
            }
            body {
                button id="sidebar-toggle" { "☰" }
                nav id="sidebar" {
                    h2 { (title) }
                    (render_channel_tree(tree, "", base_path))
                }
                main {
                    (content)
                }
            }
        }
    }
}

pub struct LogPageContext<'a> {
    pub title: &'a str,
    pub tree: &'a ChannelNode,
    pub channel: &'a Channel,
    pub date: &'a str,
    pub lines: &'a [LogLine],
    pub prev_date: Option<&'a str>,
    pub next_date: Option<&'a str>,
    pub is_today: bool,
    pub ai_enabled: bool,
    pub base_path: &'a str,
    pub search_max_len: usize,
}

pub fn log_page(ctx: &LogPageContext) -> Markup {
    let title = ctx.title;
    let tree = ctx.tree;
    let channel = ctx.channel;
    let date = ctx.date;
    let lines = ctx.lines;
    let prev_date = ctx.prev_date;
    let next_date = ctx.next_date;
    let is_today = ctx.is_today;
    let ai_enabled = ctx.ai_enabled;
    let bp = ctx.base_path;
    let encoded = channel.url_path();
    page(
        title,
        tree,
        bp,
        html! {
            header id="log-header" {
                h1 { (&channel.name) " — " (date) }
                div.nav-links {
                    @if let Some(prev) = prev_date {
                        a href=(format!("{bp}/{encoded}/{prev}")) { "← " (prev) }
                    }
                    " "
                    input id="date-picker" type="date" value=(date);
                    " "
                    @if let Some(next) = next_date {
                        a href=(format!("{bp}/{encoded}/{next}")) { (next) " →" }
                    }
                    " | "
                    a href=(format!("{bp}/{encoded}/today")) { "today" }
                    " "
                    a href=(format!("{bp}/{encoded}/{date}/raw")) { "raw" }
                }
                div.controls {
                    label {
                        input id="toggle-events" type="checkbox" checked;
                        " show events"
                    }
                    " "
                    form.search-form action=(format!("{bp}/{encoded}/search")) method="get" {
                        input type="text" name="q" placeholder="search…" maxlength=(ctx.search_max_len);
                        button type="submit" { "go" }
                    }
                    @if ai_enabled {
                        " "
                        a href=(format!("{bp}/{encoded}/ask"))
                            title="Ask an AI to search the logs and compile an answer" { "ask AI" }
                    }
                }
            }
            div id="log" data-latest=[is_today.then(|| format!("{bp}/{encoded}/latest"))] {
                @for line in lines {
                    (render_line(line))
                }
            }
        },
    )
}

pub fn search_page(
    title: &str,
    tree: &ChannelNode,
    channel: &Channel,
    query: &str,
    results: &SearchResults,
    base_path: &str,
    max_query_len: usize,
) -> Markup {
    let encoded = channel.url_path();
    page(
        title,
        tree,
        base_path,
        html! {
            header id="log-header" {
                h1 { (&channel.name) " — search" }
                div.controls {
                    form.search-form action=(format!("{base_path}/{encoded}/search")) method="get" {
                        input type="text" name="q" value=(query) placeholder="search…" maxlength=(max_query_len);
                        button type="submit" { "go" }
                    }
                }
            }
            div id="log" {
                @if let Some(days) = results.stopped_at_days {
                    p.search-note { "Searched the newest " (days) " days; older logs were not searched." }
                }
                @if results.lines.is_empty() {
                    p { "no results for \"" (query) "\"" }
                }
                @for (date, line) in &results.lines {
                    div.line {
                        a.date href=(format!("{base_path}/{encoded}/{date}#{}", line.time.to_anchor())) {
                            (date)
                        }
                        " "
                        a.ts href=(format!("{base_path}/{encoded}/{date}#{}", line.time.to_anchor())) {
                            (line.time.to_hms())
                        }
                        " "
                        @match &line.kind {
                            LineKind::Message { nick, text } => {
                                span.nick style=(nick_color_style(nick)) { "<" (nick) ">" }
                                " "
                                span.msg { (linkify(text)) }
                            },
                            LineKind::Action { nick, text } => {
                                span.action {
                                    "* "
                                    span.nick style=(nick_color_style(nick)) { (nick) }
                                    " "
                                    (linkify(text))
                                }
                            },
                            _ => {
                                span.ev { "event" }
                            },
                        }
                    }
                }
            }
        },
    )
}

pub fn ask_page(
    title: &str,
    tree: &ChannelNode,
    channel: &Channel,
    base_path: &str,
    max_query_len: usize,
) -> Markup {
    let encoded = channel.url_path();
    page(
        title,
        tree,
        base_path,
        html! {
            header id="log-header" {
                h1 { (&channel.name) " — ask AI" }
                div.nav-links {
                    a href=(format!("{base_path}/{encoded}/today")) { "back to logs" }
                }
            }
            div id="ask-container" {
                p.ask-intro {
                    "Ask a question in plain language and an AI will search the logs and compile an answer. It searches "
                    strong { (&channel.name) }
                    " by default, and can search other channels if you name them in your question."
                }
                form id="ask-form" data-stream=(format!("{base_path}/{encoded}/ask/stream")) {
                    input type="text" name="q"
                        placeholder="e.g. what was decided about the router config?"
                        maxlength=(max_query_len) autocomplete="off";
                    button type="submit" { "ask AI" }
                }
                div id="ask-links" hidden {}
                iframe id="ask-result" sandbox="" hidden {}
                div id="ask-log" {}
            }
        },
    )
}

/// Parses markdown, demoting raw HTML to visible text.
///
/// Ask documents embed verbatim IRC lines, so anyone in a logged channel can
/// plant markup; nothing here has a reason to emit HTML of its own.
fn markdown_events(content: &str) -> impl Iterator<Item = pulldown_cmark::Event<'_>> {
    use pulldown_cmark::Event;

    pulldown_cmark::Parser::new(content).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        other => other,
    })
}

pub fn ask_output_page(title: &str, md_filename: &str, content: &str, base_path: &str) -> Markup {
    let mut html_output = String::new();
    pulldown_cmark::html::push_html(&mut html_output, markdown_events(content));

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " — " (md_filename) }
                link rel="stylesheet" href=(format!("{base_path}/static/style.css"));
            }
            body {
                main.standalone {
                    div.nav-links {
                        a href=(format!("{base_path}/ask/output/{md_filename}")) { "raw" }
                    }
                    div.ask-output { (PreEscaped(html_output)) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linkify_plain() {
        let out = linkify("hello world").into_string();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn test_linkify_url() {
        let out = linkify("see https://example.com/ here").into_string();
        assert!(out.contains(r#"<a href="https://example.com/""#));
        assert!(out.contains("see "));
        assert!(out.contains(" here"));
    }

    #[test]
    fn test_linkify_url_containing_http() {
        let out = linkify("x http://ahttp://b y").into_string();
        assert_eq!(
            out,
            r#"x <a href="http://ahttp://b" target="_blank" rel="noopener">http://ahttp://b</a> y"#
        );
    }

    #[test]
    fn test_linkify_escapes_html() {
        let out = linkify("<script>alert(1)</script>").into_string();
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    fn render_markdown(content: &str) -> String {
        let mut out = String::new();
        pulldown_cmark::html::push_html(&mut out, markdown_events(content));
        out
    }

    #[test]
    fn test_markdown_escapes_block_html() {
        let out = render_markdown("<script>alert(1)</script>");
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn test_markdown_escapes_inline_html() {
        // The shape an IRC message would take inside a copied log excerpt.
        let out = render_markdown("[12:00] <nick> hi <img src=x onerror=alert(1)>");
        // The payload stays visible as text; what matters is that no tag is emitted.
        assert!(!out.contains("<img"));
        assert!(out.contains("&lt;img"));
        assert!(out.contains("&lt;nick&gt;"));
    }

    #[test]
    fn test_markdown_keeps_formatting() {
        let out = render_markdown("# Title\n\n- **bold** item\n");
        assert!(out.contains("<h1>"));
        assert!(out.contains("<strong>bold</strong>"));
        assert!(out.contains("<li>"));
    }

    #[test]
    fn test_nick_color_deterministic() {
        let h1 = nick_hue("py1hon");
        let h2 = nick_hue("py1hon");
        assert_eq!(h1, h2);
        assert_ne!(nick_hue("py1hon"), nick_hue("TiCPU"));
    }
}

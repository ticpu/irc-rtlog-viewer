use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::parser::{LineKind, LogLine};
use crate::{ChannelNode, Channel};

fn nick_hue(nick: &str) -> u16 {
    let mut hash: u32 = 5381;
    for b in nick.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u32::from(b));
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
        result.push_str(&maud::html! { a href=(url) target="_blank" rel="noopener" { (url) } }.into_string());
        last = i + url_end;
    }

    if last < text.len() {
        result.push_str(&maud::html! { (&text[last..]) }.into_string());
    }

    PreEscaped(result)
}

pub fn render_line(line: &LogLine) -> Markup {
    let anchor = line.time.to_anchor();
    let ts = line.time.to_hms();
    let class = if line.is_event() { "line event" } else { "line" };

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

fn render_channel_tree(node: &ChannelNode, base_path: &str) -> Markup {
    html! {
        ul {
            @for (name, child) in &node.children {
                li {
                    @let child_path = if base_path.is_empty() {
                        name.clone()
                    } else {
                        format!("{base_path}/{name}")
                    };
                    @let encoded_path = child_path.replace('#', "%23");
                    @if let Some(channel) = &child.channel {
                        a href=(format!("/{}/today", encoded_path)) { (&channel.name) }
                    } @else {
                        span.tree-label { (name) }
                    }
                    @if !child.children.is_empty() {
                        (render_channel_tree(child, &child_path))
                    }
                }
            }
        }
    }
}

pub fn page(title: &str, tree: &ChannelNode, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                nav id="sidebar" {
                    h2 { (title) }
                    (render_channel_tree(tree, ""))
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
    let encoded = channel.path_segments.join("/").replace('#', "%23");
    page(title, tree, html! {
        header id="log-header" {
            h1 { (&channel.name) " — " (date) }
            div.nav-links {
                @if let Some(prev) = prev_date {
                    a href=(format!("/{encoded}/{prev}")) { "← " (prev) }
                }
                " "
                input type="date" value=(date)
                    onchange="window.location.href=this.value"
                    ;
                " "
                @if let Some(next) = next_date {
                    a href=(format!("/{encoded}/{next}")) { (next) " →" }
                }
                " | "
                a href=(format!("/{encoded}/today")) { "today" }
                " "
                a href=(format!("/{encoded}/{date}/raw")) { "raw" }
            }
            div.controls {
                label {
                    input id="toggle-events" type="checkbox" checked;
                    " show events"
                }
                " "
                form.search-form action=(format!("/{encoded}/search")) method="get" {
                    input type="text" name="q" placeholder="search…";
                    button type="submit" { "go" }
                }
            }
        }
        div id="log" data-channel=(&encoded) {
            @for line in lines {
                (render_line(line))
            }
        }
        @if is_today {
            script {
                (PreEscaped(r#"
(function() {
    var log = document.getElementById('log');
    var src = new EventSource('/' + log.dataset.channel + '/latest');
    var atBottom = true;
    window.addEventListener('scroll', function() {
        atBottom = (window.innerHeight + window.scrollY) >= (document.body.offsetHeight - 50);
    });
    src.onmessage = function(e) {
        log.insertAdjacentHTML('beforeend', e.data);
        if (atBottom) window.scrollTo(0, document.body.scrollHeight);
    };
    var cb = document.getElementById('toggle-events');
    cb.addEventListener('change', function() {
        log.classList.toggle('hide-events', !cb.checked);
    });
})();
"#))
            }
        } @else {
            script {
                (PreEscaped(r#"
(function() {
    var cb = document.getElementById('toggle-events');
    var log = document.getElementById('log');
    cb.addEventListener('change', function() {
        log.classList.toggle('hide-events', !cb.checked);
    });
})();
"#))
            }
        }
    })
}

pub fn search_page(
    title: &str,
    tree: &ChannelNode,
    channel: &Channel,
    query: &str,
    results: &[(String, LogLine)],
) -> Markup {
    let encoded = channel.path_segments.join("/").replace('#', "%23");
    page(title, tree, html! {
        header id="log-header" {
            h1 { (&channel.name) " — search" }
            div.controls {
                form.search-form action=(format!("/{encoded}/search")) method="get" {
                    input type="text" name="q" value=(query) placeholder="search…";
                    button type="submit" { "go" }
                }
            }
        }
        div id="log" {
            @if results.is_empty() {
                p { "no results for \"" (query) "\"" }
            }
            @for (date, line) in results {
                div.line {
                    a.date href=(format!("/{encoded}/{date}#{}", line.time.to_anchor())) {
                        (date)
                    }
                    " "
                    a.ts href=(format!("/{encoded}/{date}#{}", line.time.to_anchor())) {
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
    })
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
    fn test_linkify_escapes_html() {
        let out = linkify("<script>alert(1)</script>").into_string();
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn test_nick_color_deterministic() {
        let h1 = nick_hue("py1hon");
        let h2 = nick_hue("py1hon");
        assert_eq!(h1, h2);
        assert_ne!(nick_hue("py1hon"), nick_hue("TiCPU"));
    }
}

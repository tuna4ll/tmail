//! Turns a message written in Markdown into HTML that mail programs show well: inline
//! styles instead of a style sheet, coloured code blocks, and line breaks kept the
//! way they were typed.

use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
use syntect::{highlighting::ThemeSet, parsing::SyntaxSet};

fn syntaxes() -> &'static (SyntaxSet, ThemeSet) {
    static SETS: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    SETS.get_or_init(|| {
        (
            SyntaxSet::load_defaults_newlines(),
            ThemeSet::load_defaults(),
        )
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A fenced block in the language its fence names, coloured with inline styles.
fn code_block(language: &str, code: &str) -> String {
    let (syntaxes, themes) = syntaxes();
    let syntax = syntaxes
        .find_syntax_by_token(language)
        .filter(|_| !language.is_empty());
    let block_style = "padding:12px;border-radius:6px;overflow-x:auto;font-family:ui-monospace,Menlo,Consolas,monospace;font-size:13px;line-height:1.45";
    match syntax {
        Some(syntax) => {
            let theme = &themes.themes["InspiredGitHub"];
            match syntect::html::highlighted_html_for_string(code, syntaxes, syntax, theme) {
                Ok(html) => html.replacen(
                    "<pre style=\"",
                    &format!("<pre style=\"{block_style};border:1px solid #d0d7de;"),
                    1,
                ),
                Err(_) => plain_block(code, block_style),
            }
        }
        None => plain_block(code, block_style),
    }
}

fn plain_block(code: &str, style: &str) -> String {
    format!(
        "<pre style=\"{style};background:#f6f8fa;border:1px solid #d0d7de\"><code>{}</code></pre>\n",
        escape(code)
    )
}

/// The HTML part for `source`. The signature after `-- ` stays as typed.
pub fn to_html(source: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let (text, signature) = match source.find("\n-- \n") {
        Some(at) => (&source[..at], Some(&source[at + 1..])),
        None if source.starts_with("-- \n") => ("", Some(source.as_str())),
        None => (source.as_str(), None),
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut events = Vec::new();
    let mut code: Option<(String, String)> = None;
    for event in Parser::new_ext(text, options) {
        match (&mut code, event) {
            (None, Event::Start(Tag::CodeBlock(kind))) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or_default().to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((language, String::new()));
            }
            (Some((_, buffer)), Event::Text(text)) => buffer.push_str(&text),
            (Some(_), Event::End(TagEnd::CodeBlock)) => {
                let (language, buffer) = code.take().unwrap_or_default();
                events.push(Event::Html(code_block(&language, &buffer).into()));
            }
            (Some(_), _) => {}
            // Mail is written line by line; a new line in the text is a new line in
            // the message, as in chat apps and code review comments.
            (None, Event::SoftBreak) => events.push(Event::HardBreak),
            (None, Event::Code(text)) => events.push(Event::Html(
                format!(
                    "<code style=\"background:#eff1f3;padding:1px 4px;border-radius:4px;font-family:ui-monospace,Menlo,Consolas,monospace;font-size:90%\">{}</code>",
                    escape(&text)
                )
                .into(),
            )),
            (None, event) => events.push(event),
        }
    }
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    let body = body
        .replace("<table>", "<table style=\"border-collapse:collapse;margin:8px 0\">")
        .replace("<th>", "<th style=\"border:1px solid #d0d7de;padding:6px 12px;background:#f6f8fa;text-align:left\">")
        .replace("<th style=\"text-align: ", "<th style=\"border:1px solid #d0d7de;padding:6px 12px;background:#f6f8fa;text-align: ")
        .replace("<td>", "<td style=\"border:1px solid #d0d7de;padding:6px 12px\">")
        .replace("<td style=\"text-align: ", "<td style=\"border:1px solid #d0d7de;padding:6px 12px;text-align: ")
        .replace("<blockquote>", "<blockquote style=\"margin:0 0 0 4px;padding:0 12px;border-left:3px solid #d0d7de;color:#57606a\">");
    let mut output = String::from(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"></head>\n<body><div style=\"font-family:-apple-system,'Segoe UI',Helvetica,Arial,sans-serif;font-size:15px;line-height:1.5;color:#1f2328\">\n",
    );
    output.push_str(&body);
    if let Some(signature) = signature {
        output.push_str(&format!(
            "<div style=\"color:#57606a;white-space:pre-wrap\">{}</div>\n",
            escape(signature.trim_end())
        ));
    }
    output.push_str("</div></body></html>\n");
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_mail_friendly_html() {
        let html = super::to_html(
            "# Plan\nfirst line\nsecond line\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n\n> quoted <b>\n\n-- \nAda\n",
        );
        assert!(html.contains("<h1>Plan</h1>"));
        assert!(html.contains("first line<br />"), "{html}");
        assert!(html.contains("border-collapse"));
        assert!(html.contains("<pre style=") && html.contains("main"));
        assert!(!html.contains("```"));
        assert!(
            html.contains("white-space:pre-wrap\">-- \nAda</div>"),
            "{html}"
        );
    }
}

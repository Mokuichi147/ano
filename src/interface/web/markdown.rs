//! The model's Markdown answers as HTML for the browser. Raw HTML in an
//! answer is shown as text, and links only keep web and mail addresses, so
//! an answer cannot run script in the page. The page's Content-Security-Policy
//! also keeps images from loading from other hosts.

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag};

pub(super) fn render(markdown: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        event => event,
    });
    let mut output = String::new();
    html::push_html(&mut output, events);
    output
}

/// `url` when it is relative or a web or mail address, `#` otherwise (such
/// as `javascript:`).
fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.trim().to_ascii_lowercase())
        .filter(|scheme| !scheme.contains(['/', '?', '#']));
    match scheme.as_deref() {
        None | Some("http" | "https" | "mailto") => url,
        Some(_) => CowStr::Borrowed("#"),
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn raw_html_is_text_and_script_links_are_dropped() {
        let html = render("**bold** <script>alert(1)</script>\n\n[a](javascript:alert(1)) [b](https://example.com/x?y=1:2) [c](./notes.md)");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains(r##"<a href="#">a</a>"##), "{html}");
        assert!(
            html.contains(r#"href="https://example.com/x?y=1:2""#),
            "{html}"
        );
        assert!(html.contains(r#"href="./notes.md""#), "{html}");
        let block = render("<div onclick=\"x()\">hi</div>\n");
        assert!(!block.contains("<div"), "{block}");
    }
}

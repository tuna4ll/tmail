//! Lays out a message body as terminal rows: HTML keeps its emphasis, colours, links
//! and tables, and pictures sit between the text rows they appear in.

use std::path::Path;

use html2text::render::RichAnnotation;
use image::DynamicImage;
use mail_parser::{MessageParser, MimeHeaders};
use ratatui::{
    layout::Size,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};
use unicode_width::UnicodeWidthChar;

use crate::{mail::Mail, theme};

/// Tallest a picture may be, in rows, so it never pushes the text out of view.
const MAX_IMAGE_ROWS: u16 = 24;
/// Images this small or smaller are tracking pixels.
const PIXEL: u32 = 2;
const MARK_START: char = '\u{E000}';
const MARK_END: char = '\u{E001}';
/// Pictures beyond these limits are shown by their alt text instead of decoded.
const MAX_DIMENSION: u32 = 8192;
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
/// Remote pictures larger than this are not downloaded.
const MAX_REMOTE_BYTES: u64 = 8 * 1024 * 1024;

pub enum Picture {
    /// Encoded bytes that have not been decoded yet.
    Bytes(Vec<u8>),
    Ready(DynamicImage),
    /// A remote picture that is only fetched on request.
    Blocked(String),
    Loading,
    Failed,
}

pub struct Slot {
    pub alt: String,
    pub picture: Picture,
    protocol: Option<(Size, Protocol)>,
}

impl Slot {
    fn new(alt: String, picture: Picture) -> Self {
        Self {
            alt,
            picture,
            protocol: None,
        }
    }

    fn decoded(&mut self) -> Option<&DynamicImage> {
        if let Picture::Bytes(bytes) = &self.picture {
            self.picture = image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .ok()
                .and_then(|mut reader| {
                    // Huge pictures would stall the interface while they decode.
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(MAX_DIMENSION);
                    limits.max_image_height = Some(MAX_DIMENSION);
                    limits.max_alloc = Some(MAX_DECODED_BYTES);
                    reader.limits(limits);
                    reader.decode().ok()
                })
                .map_or(Picture::Failed, Picture::Ready);
        }
        match &self.picture {
            Picture::Ready(image) if image.width() > PIXEL || image.height() > PIXEL => Some(image),
            _ => None,
        }
    }

    fn label(&self) -> String {
        let alt = if self.alt.trim().is_empty() {
            "image"
        } else {
            self.alt.trim()
        };
        match &self.picture {
            Picture::Blocked(_) => format!("[{alt} · press I to load remote images]"),
            Picture::Loading => format!("[{alt} · loading…]"),
            _ => format!("[{alt}]"),
        }
    }
}

pub enum Row {
    Line(Line<'static>),
    /// The first row of a picture that is `height` rows tall.
    Image {
        slot: usize,
        height: u16,
    },
    /// A row covered by the picture that starts at `start`.
    Covered {
        start: usize,
    },
}

#[derive(Default)]
pub struct Body {
    html: Option<String>,
    text: String,
    pub slots: Vec<Slot>,
    /// Pictures attached to the message rather than placed in its HTML.
    trailing: Vec<usize>,
}

impl Body {
    /// Reads the message file for its HTML and pictures, falling back to the cached text.
    pub fn load(mail: &Mail) -> Self {
        let mut body = Self {
            text: mail.body.clone(),
            ..Self::default()
        };
        let Some(contents) = mail
            .source_path
            .as_deref()
            .and_then(|path: &Path| std::fs::read(path).ok())
        else {
            return body;
        };
        let Some(message) = MessageParser::default().parse(&contents) else {
            return body;
        };
        let mut referenced = Vec::new();
        if let Some(html) = message.body_html(0) {
            let (html, slots) = place_images(&html, |id| {
                message.parts.iter().enumerate().find_map(|(index, part)| {
                    (part.content_id().map(clean_id) == Some(id)).then(|| {
                        referenced.push(index);
                        part.contents().to_vec()
                    })
                })
            });
            body.html = Some(html);
            body.slots = slots;
        }
        for (index, part) in message.parts.iter().enumerate() {
            let is_image = part
                .content_type()
                .is_some_and(|kind| kind.c_type.eq_ignore_ascii_case("image"));
            if is_image && !referenced.contains(&index) && !part.contents().is_empty() {
                body.trailing.push(body.slots.len());
                body.slots.push(Slot::new(
                    part.attachment_name().unwrap_or("image").to_owned(),
                    Picture::Bytes(part.contents().to_vec()),
                ));
            }
        }
        body
    }

    pub fn has_blocked(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot.picture, Picture::Blocked(_)))
    }

    /// Marks remote pictures as loading and returns their addresses.
    pub fn start_remote(&mut self) -> Vec<(usize, String)> {
        let mut requests = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Picture::Blocked(url) = &slot.picture {
                requests.push((index, url.clone()));
                slot.picture = Picture::Loading;
            }
        }
        requests
    }

    pub fn finish_remote(&mut self, index: usize, bytes: Option<Vec<u8>>) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.picture = bytes.map_or(Picture::Failed, Picture::Bytes);
            slot.protocol = None;
        }
    }

    pub fn rows(&mut self, width: u16, picker: Option<&Picker>) -> Vec<Row> {
        let mut rows = Vec::new();
        let html = self.html.clone();
        let rendered = html.as_deref().and_then(|html| {
            html2text::config::rich()
                .use_doc_css()
                .no_table_borders()
                .allow_width_overflow()
                .link_footnotes(false)
                .lines_from_read(html.as_bytes(), usize::from(width.max(8)))
                .ok()
        });
        match rendered {
            Some(lines) => {
                for line in lines {
                    let mut spans: Vec<Span<'static>> = line
                        .tagged_strings()
                        .map(|tagged| Span::styled(tagged.s.clone(), style_for(&tagged.tag)))
                        .collect();
                    heading(&mut spans);
                    self.push_marked(Line::from(spans), width, picker, &mut rows);
                }
            }
            None => {
                for line in self.text.lines() {
                    for line in wrap(Line::from(line.to_owned()), width) {
                        rows.push(Row::Line(line));
                    }
                }
            }
        }
        for index in self.trailing.clone() {
            rows.push(Row::Line(Line::default()));
            self.push_image(index, width, picker, &mut rows);
        }
        rows
    }

    /// Splits a line at picture markers, putting each picture on its own rows.
    fn push_marked(
        &mut self,
        line: Line<'static>,
        width: u16,
        picker: Option<&Picker>,
        rows: &mut Vec<Row>,
    ) {
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut pictures = Vec::new();
        for span in line.spans {
            let mut rest = span.content.as_ref();
            while let Some(start) = rest.find(MARK_START) {
                let Some(end) = rest[start..].find(MARK_END) else {
                    break;
                };
                if start > 0 {
                    current.push(Span::styled(rest[..start].to_owned(), span.style));
                }
                if let Ok(index) = rest[start + MARK_START.len_utf8()..start + end].parse() {
                    pictures.push(index);
                }
                rest = &rest[start + end + MARK_END.len_utf8()..];
            }
            if !rest.is_empty() {
                current.push(Span::styled(rest.to_owned(), span.style));
            }
        }
        let text = Line::from(current);
        if pictures.is_empty()
            || text
                .spans
                .iter()
                .any(|span| !span.content.trim().is_empty())
        {
            for line in wrap(text, width) {
                rows.push(Row::Line(line));
            }
        }
        for index in pictures {
            self.push_image(index, width, picker, rows);
        }
    }

    fn push_image(
        &mut self,
        index: usize,
        width: u16,
        picker: Option<&Picker>,
        rows: &mut Vec<Row>,
    ) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        let label = slot.label();
        let protocol = picker.and_then(|picker| {
            let image = slot.decoded()?.clone();
            let available = Size::new(width, MAX_IMAGE_ROWS);
            let size = Resize::Fit(None).size_for(&image, picker.font_size(), available);
            if size.width == 0 || size.height == 0 {
                return None;
            }
            if !slot
                .protocol
                .as_ref()
                .is_some_and(|(cached, _)| *cached == size)
            {
                let protocol = picker.new_protocol(image, size, Resize::Fit(None)).ok()?;
                slot.protocol = Some((size, protocol));
            }
            Some(size.height)
        });
        match protocol {
            Some(height) => {
                let start = rows.len();
                rows.push(Row::Image {
                    slot: index,
                    height,
                });
                rows.extend((1..height).map(|_| Row::Covered { start }));
            }
            None if matches!(slot.picture, Picture::Ready(_)) && slot.decoded().is_none() => {}
            None => rows.push(Row::Line(Line::styled(label, theme::muted()))),
        }
    }

    pub fn protocol(&self, slot: usize) -> Option<&Protocol> {
        self.slots
            .get(slot)?
            .protocol
            .as_ref()
            .map(|(_, protocol)| protocol)
    }
}

/// Writes the message's HTML to a private temporary file for a web browser, with
/// inline pictures embedded so the page is complete on its own.
pub fn export_html(mail: &Mail) -> std::io::Result<std::path::PathBuf> {
    let missing = || std::io::Error::other("This message has no HTML part.");
    let path = mail.source_path.as_deref().ok_or_else(missing)?;
    let contents = std::fs::read(path)?;
    let message = MessageParser::default()
        .parse(&contents)
        .ok_or_else(missing)?;
    let mut html = message.body_html(0).ok_or_else(missing)?.into_owned();
    for part in &message.parts {
        let (Some(id), Some(kind)) = (part.content_id(), part.content_type()) else {
            continue;
        };
        let reference = format!("cid:{}", clean_id(id));
        if html.contains(&reference) {
            let media_type = format!(
                "{}/{}",
                kind.c_type,
                kind.c_subtype.as_deref().unwrap_or("octet-stream")
            );
            let data = format!("data:{media_type};base64,{}", base64(part.contents()));
            html = html.replace(&reference, &data);
        }
    }
    prune_exports();
    let path = std::env::temp_dir().join(format!(
        "{}.html",
        crate::storage::unique_name(EXPORT_PREFIX)
    ));
    crate::storage::write_new(&path, html.as_bytes())?;
    Ok(path)
}

const EXPORT_PREFIX: &str = "termail-view";

/// Removes pages exported for the browser more than a day ago.
fn prune_exports() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 24 * 60 * 60);
        if old
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(EXPORT_PREFIX)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..4 {
            output.push(if index <= chunk.len() {
                char::from(TABLE[((value >> (18 - 6 * index)) & 63) as usize])
            } else {
                '='
            });
        }
    }
    output
}

/// Fetches a remote picture with a short timeout and a size cap.
pub fn fetch(url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return None;
    }
    let agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent();
    let mut response = agent.get(url).call().ok()?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_REMOTE_BYTES)
        .read_to_vec()
        .ok()
}

fn clean_id(value: &str) -> &str {
    value.trim().trim_start_matches('<').trim_end_matches('>')
}

/// Replaces `<img>` tags with markers and collects their pictures. Tracking pixels and
/// images without a usable source keep only their alt text.
fn place_images(
    html: &str,
    mut inline: impl FnMut(&str) -> Option<Vec<u8>>,
) -> (String, Vec<Slot>) {
    let mut output = String::with_capacity(html.len());
    let mut slots = Vec::new();
    let mut rest = html;
    while let Some(start) = find_tag(rest, "img") {
        output.push_str(&rest[..start]);
        let Some(length) = tag_length(&rest[start..]) else {
            output.push_str(&rest[start..]);
            return (output, slots);
        };
        let attributes = attributes(&rest[start + 4..start + length - 1]);
        let attribute = |name: &str| {
            attributes
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let tiny = ["width", "height"].iter().any(|name| {
            attribute(name)
                .and_then(|value| value.trim().trim_end_matches("px").parse::<u32>().ok())
                .is_some_and(|value| value <= PIXEL)
        });
        let alt = attribute("alt").unwrap_or_default().to_owned();
        let src = attribute("src").unwrap_or_default().trim();
        let picture = if tiny {
            None
        } else if let Some(id) = src.strip_prefix("cid:") {
            inline(clean_id(id)).map(Picture::Bytes)
        } else if let Some(data) = src.strip_prefix("data:") {
            data.split_once(";base64,")
                .filter(|(kind, _)| kind.starts_with("image/"))
                .and_then(|(_, encoded)| {
                    let encoded: Vec<u8> = encoded
                        .bytes()
                        .filter(|byte| !byte.is_ascii_whitespace())
                        .collect();
                    mail_parser::decoders::base64::base64_decode(&encoded)
                })
                .map(Picture::Bytes)
        } else if src.starts_with("https://") || src.starts_with("http://") {
            Some(Picture::Blocked(src.to_owned()))
        } else {
            None
        };
        match picture {
            Some(picture) => {
                output.push(MARK_START);
                output.push_str(&slots.len().to_string());
                output.push(MARK_END);
                slots.push(Slot::new(alt, picture));
            }
            None if !tiny && !alt.trim().is_empty() => {
                output.push('[');
                output.push_str(&escape(&alt));
                output.push(']');
            }
            None => {}
        }
        rest = &rest[start + length..];
    }
    output.push_str(rest);
    (output, slots)
}

/// Finds the next `<name` opening tag, case-insensitively.
fn find_tag(value: &str, name: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut position = 0;
    while let Some(offset) = value[position..].find('<') {
        let start = position + offset;
        let after = start + 1 + name.len();
        if bytes.len() > after
            && bytes[start + 1..after].eq_ignore_ascii_case(name.as_bytes())
            && matches!(bytes[after], b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>')
        {
            return Some(start);
        }
        position = start + 1;
    }
    None
}

/// The length of a tag up to and including its `>`, skipping quoted values.
fn tag_length(value: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in value.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if open == character => quote = None,
            (None, '>') => return Some(index + 1),
            _ => {}
        }
    }
    None
}

fn attributes(value: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut chars = value.trim_end_matches('/').chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace() || *c == '/') {
            chars.next();
        }
        let name: String =
            std::iter::from_fn(|| chars.next_if(|c| !c.is_whitespace() && *c != '=')).collect();
        if name.is_empty() {
            break;
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.next_if_eq(&'=').is_some() {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            match chars.peek().copied() {
                Some(quote @ ('"' | '\'')) => {
                    chars.next();
                    value = std::iter::from_fn(|| chars.next_if(|c| *c != quote)).collect();
                    chars.next();
                }
                _ => {
                    value = std::iter::from_fn(|| chars.next_if(|c| !c.is_whitespace())).collect();
                }
            }
        }
        attributes.push((name, unescape(&value)));
    }
    attributes
}

fn unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Turns html2text's `## ` heading prefix into bold text.
fn heading(spans: &mut [Span<'static>]) {
    let Some(first) = spans.first_mut() else {
        return;
    };
    let hashes = first.content.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 || !first.content[hashes..].starts_with(' ') {
        return;
    }
    first.content = first.content[hashes + 1..].to_owned().into();
    for span in spans.iter_mut() {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
}

fn rgb(colour: &html2text::Colour) -> (u8, u8, u8) {
    (colour.r, colour.g, colour.b)
}

/// Relative luminance per WCAG.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The reader's background, as the RGB value of `theme::SURFACE`.
const SURFACE_RGB: (u8, u8, u8) = (48, 48, 48);

fn style_for(tags: &[RichAnnotation]) -> Style {
    let mut style = theme::text();
    let mut foreground = None;
    let mut background = None;
    for tag in tags {
        match tag {
            RichAnnotation::Strong => style = style.add_modifier(Modifier::BOLD),
            RichAnnotation::Emphasis => style = style.add_modifier(Modifier::ITALIC),
            RichAnnotation::Strikeout => style = style.add_modifier(Modifier::CROSSED_OUT),
            RichAnnotation::Code | RichAnnotation::Preformat(_) => {
                style = style.fg(theme::ACCENT);
            }
            RichAnnotation::Link(_) => {
                style = style.fg(theme::ACCENT).add_modifier(Modifier::UNDERLINED);
            }
            RichAnnotation::Colour(colour) => foreground = Some(rgb(colour)),
            RichAnnotation::BgColour(colour) => background = Some(rgb(colour)),
            _ => {}
        }
    }
    // Page and card backgrounds are white, black or grey; painting them behind each run
    // of text would leave stripes, so only coloured highlights and buttons are kept.
    let background = background.filter(|(r, g, b)| r.max(g).max(b) - r.min(g).min(b) >= 32);
    let behind = background.unwrap_or(SURFACE_RGB);
    if let Some(colour) = background {
        style = style.bg(Color::Rgb(colour.0, colour.1, colour.2));
    }
    match foreground {
        Some(colour) if contrast(colour, behind) >= 3.0 => {
            style = style.fg(Color::Rgb(colour.0, colour.1, colour.2));
        }
        _ if background.is_some() && contrast((208, 208, 208), behind) < 3.0 => {
            style = style.fg(Color::Black);
        }
        _ => {}
    }
    style
}

/// Word-wraps a styled line to `width` columns, breaking long words where needed.
pub fn wrap(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let style = line.style;
    let mut words: Vec<(String, Style)> = Vec::new();
    for span in &line.spans {
        let mut word = String::new();
        for character in span.content.chars() {
            word.push(character);
            if character == ' ' {
                words.push((std::mem::take(&mut word), span.style));
            }
        }
        if !word.is_empty() {
            words.push((word, span.style));
        }
    }
    for (word, word_style) in words {
        let word_width: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
        let trimmed: usize = word
            .trim_end()
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum();
        if used > 0 && used + trimmed > width {
            lines.push(Line::from(std::mem::take(&mut current)).style(style));
            used = 0;
        }
        if word_width <= width.saturating_sub(used) || trimmed <= width.saturating_sub(used) {
            used += word_width;
            current.push(Span::styled(word, word_style));
            continue;
        }
        // A word longer than the line: break it by columns.
        let mut piece = String::new();
        for character in word.chars() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width > width && used > 0 {
                current.push(Span::styled(std::mem::take(&mut piece), word_style));
                lines.push(Line::from(std::mem::take(&mut current)).style(style));
                used = 0;
            }
            piece.push(character);
            used += character_width;
        }
        if !piece.is_empty() {
            current.push(Span::styled(piece, word_style));
        }
    }
    lines.push(Line::from(current).style(style));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_images_and_drops_tracking_pixels() {
        let html = r#"<p>Hi<IMG src="cid:logo@x" alt='Logo'><img src="https://t.example/p.gif" width="1" height="1"><img
            alt="Chart &amp; data" src="https://example.test/c.png"/><img src="file:///etc/x" alt="local"></p>"#;
        let (output, slots) = place_images(html, |id| (id == "logo@x").then(|| vec![1, 2, 3]));
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].alt, "Logo");
        assert!(matches!(slots[0].picture, Picture::Bytes(ref bytes) if bytes == &[1, 2, 3]));
        assert_eq!(slots[1].alt, "Chart & data");
        assert!(
            matches!(slots[1].picture, Picture::Blocked(ref url) if url == "https://example.test/c.png")
        );
        assert!(!output.contains("t.example"));
        assert!(output.contains("[local]"));
        assert_eq!(output.matches(MARK_START).count(), 2);
    }

    #[test]
    fn styles_follow_html_and_stay_readable() {
        let mut body = Body {
            html: Some(
                r#"<p><b>bold</b> <i>it</i> <a href="https://x.test">link</a>
                <span style="color:#333333">dark</span> <span style="color:#ff5555">red</span></p>"#
                    .into(),
            ),
            ..Body::default()
        };
        let rows = body.rows(60, None);
        let Row::Line(line) = &rows[0] else { panic!() };
        let style = |text: &str| {
            line.spans
                .iter()
                .find(|span| span.content.contains(text))
                .unwrap()
                .style
        };
        assert!(style("bold").add_modifier.contains(Modifier::BOLD));
        assert!(style("it").add_modifier.contains(Modifier::ITALIC));
        assert!(style("link").add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(style("dark").fg, Some(theme::TEXT));
        assert_eq!(style("red").fg, Some(Color::Rgb(255, 85, 85)));
    }

    #[test]
    fn wraps_styled_words() {
        let line = Line::from(vec![
            Span::raw("hello "),
            Span::styled("wonderful world", Style::new().bold()),
        ]);
        let lines = wrap(line, 11);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].spans[0].content, "wonderful ");
        assert_eq!(lines[2].spans[0].content, "world");
        assert!(
            lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(wrap(Line::from("abcdefghij"), 4).len(), 3);
    }
}

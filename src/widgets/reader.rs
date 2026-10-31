use crate::{
    mail::Mail,
    theme,
    widgets::{
        html::{LinkArea, Placed},
        rich::{Body, wrap},
    },
};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use ratatui_image::{Image, picker::Picker};

/// Something in the message that reacts to a click.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Link(String),
    Attachment(usize),
}

/// The message laid out for one width: header and body rows, pictures and links.
#[derive(Default)]
struct Laid {
    /// Columns the rows take, at least the window's.
    width: u16,
    lines: Vec<Line<'static>>,
    images: Vec<Placed>,
    links: Vec<LinkArea>,
}

pub struct Reader {
    mail_id: String,
    scroll: u16,
    max_scroll: u16,
    body: Option<Body>,
    /// Laid-out rows and the width they were laid out for.
    rows: Option<(u16, Laid)>,
    /// Layouts for other widths, so going back to a size is instant.
    cache: Vec<(u16, Laid)>,
    /// While the window is being resized, the last layout is shown until this time.
    defer_until: Option<std::time::Instant>,
    hscroll: u16,
    max_hscroll: u16,
    drew_images: bool,
    /// Decrypted or verified content that replaces the file's.
    opened: Option<Vec<u8>>,
    /// The signature or encryption state, once known.
    security: Option<String>,
    /// Laid-out row indexes that open a link or an attachment.
    targets: Vec<(usize, Target)>,
    /// Where the frame and the text were last drawn.
    frame_area: Rect,
    text_area: Rect,
}

impl Reader {
    pub fn new(mail_id: String) -> Self {
        Self {
            mail_id,
            scroll: 0,
            max_scroll: 0,
            body: None,
            rows: None,
            cache: Vec::new(),
            defer_until: None,
            hscroll: 0,
            max_hscroll: 0,
            drew_images: false,
            opened: None,
            security: None,
            targets: Vec::new(),
            frame_area: Rect::default(),
            text_area: Rect::default(),
        }
    }

    /// Whether a screen cell is inside the reader.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.frame_area
            .contains(ratatui::layout::Position::new(column, row))
    }

    /// The link or attachment drawn at a screen cell.
    pub fn target_at(&self, column: u16, row: u16) -> Option<Target> {
        if !self
            .text_area
            .contains(ratatui::layout::Position::new(column, row))
        {
            return None;
        }
        let index = usize::from(self.scroll) + usize::from(row - self.text_area.y);
        let column = column - self.text_area.x + self.hscroll;
        if let Some((_, laid)) = &self.rows
            && let Some(link) = laid
                .links
                .iter()
                .find(|link| link.y == index && column >= link.start && column < link.end)
        {
            return Some(Target::Link(link.url.clone()));
        }
        self.targets
            .iter()
            .find(|(target_row, _)| *target_row == index)
            .map(|(_, target)| target.clone())
    }

    /// Shows decrypted or verified content in place of the stored message.
    pub fn set_opened(&mut self, content: Vec<u8>, status: String) {
        self.opened = Some(content);
        self.security = Some(status);
        self.body = None;
        self.forget_layouts();
    }

    pub fn set_security(&mut self, status: String) {
        self.security = Some(status);
        self.forget_layouts();
    }

    pub fn mail_id(&self) -> &str {
        &self.mail_id
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount).min(self.max_scroll);
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    /// Whether the last frame placed terminal graphics, which need a full redraw to
    /// erase once they move or close.
    pub fn shows_images(&self) -> bool {
        self.drew_images
    }

    /// The message body with its pictures, read from the message file on first use.
    pub fn body(&mut self, mail: &Mail) -> &mut Body {
        let opened = self.opened.as_deref();
        self.body
            .get_or_insert_with(|| Body::load_from(mail, opened))
    }

    /// Lays the message out again, after pictures arrive.
    pub fn invalidate(&mut self) {
        self.forget_layouts();
    }

    fn forget_layouts(&mut self) {
        self.rows = None;
        self.cache.clear();
    }

    /// Keeps showing the current layout while the window changes size, and lays it
    /// out again once it has been still for a moment.
    pub fn defer_layout(&mut self) {
        self.defer_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
    }

    pub fn scroll_left(&mut self, amount: u16) {
        self.hscroll = self.hscroll.saturating_sub(amount);
    }

    pub fn scroll_right(&mut self, amount: u16) {
        self.hscroll = self.hscroll.saturating_add(amount).min(self.max_hscroll);
    }

    /// Whether the body is wider than the window.
    pub fn scrolls_sideways(&self) -> bool {
        self.max_hscroll > 0
    }

    /// Scrolls so that `row` of the scroll bar's `height` rows is in view.
    pub fn scroll_to_fraction(&mut self, row: u16, height: u16) {
        let height = height.max(1);
        let target = u32::from(self.max_scroll) * u32::from(row.min(height - 1))
            / u32::from((height - 1).max(1));
        self.scroll = target as u16;
    }

    /// The column of the vertical scroll bar and the rows it covers, when shown.
    pub fn scrollbar(&self) -> Option<(u16, u16, u16)> {
        (self.max_scroll > 0 && self.frame_area.height > 2).then(|| {
            (
                self.frame_area.right().saturating_sub(1),
                self.frame_area.y + 1,
                self.frame_area.height - 2,
            )
        })
    }

    /// Switches HTML between its own colours and the reader's. Returns whether the
    /// message's own colours are now shown.
    pub fn toggle_colors(&mut self, mail: &Mail) -> bool {
        let body = self.body(mail);
        body.original_colors = !body.original_colors;
        let original = body.original_colors;
        self.forget_layouts();
        original
    }

    pub fn is_html(&mut self, mail: &Mail) -> bool {
        self.body(mail).is_html()
    }

    /// The outer frame drawn last, for moving and resizing the window with the mouse.
    pub fn frame_area(&self) -> Rect {
        self.frame_area
    }

    /// Draws the reader as a window at `area`.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, mail: &Mail, picker: Option<&Picker>) {
        self.render(frame, area, mail, picker, true);
    }

    pub fn draw_docked(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        mail: &Mail,
        picker: Option<&Picker>,
    ) {
        self.render(frame, area, mail, picker, false);
    }

    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        mail: &Mail,
        picker: Option<&Picker>,
        clear: bool,
    ) {
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::border(true))
            .title_style(theme::title(true))
            .bg(theme::surface());
        if clear {
            // A window shows what it holds and how to handle it.
            let room = usize::from(area.width.saturating_sub(40)).max(8);
            let subject: String = mail.subject.chars().take(room).collect();
            block = block.title(format!(" {subject} ")).title_top(
                Line::from(" f full · +/- size · t colours ")
                    .style(theme::muted())
                    .right_aligned(),
            );
        } else {
            block = block.title(" message ");
        }
        let inner = block.inner(area);
        self.frame_area = area;
        self.text_area = inner;
        if clear {
            frame.render_widget(Clear, area);
        }
        frame.render_widget(block, area);
        let width = inner.width;
        let deferring = self.rows.is_some()
            && self
                .defer_until
                .is_some_and(|until| std::time::Instant::now() < until);
        if !deferring {
            self.defer_until = None;
        }
        if !deferring
            && self
                .rows
                .as_ref()
                .is_none_or(|(laid_out, _)| *laid_out != width)
        {
            let cached = self
                .cache
                .iter()
                .position(|(laid_out, _)| *laid_out == width);
            let laid = match cached {
                Some(index) => self.cache.remove(index).1,
                None => self.layout(mail, width, picker),
            };
            if let Some(previous) = self.rows.replace((width, laid)) {
                self.cache.insert(0, previous);
                self.cache.truncate(4);
            }
        }
        let Some((_, laid)) = &self.rows else {
            return;
        };
        let viewport = usize::from(inner.height);
        self.max_scroll = laid.lines.len().saturating_sub(viewport) as u16;
        self.scroll = self.scroll.min(self.max_scroll);
        self.max_hscroll = laid.width.saturating_sub(width);
        self.hscroll = self.hscroll.min(self.max_hscroll);
        let scroll = usize::from(self.scroll);
        let shift = usize::from(self.hscroll);
        for (offset, line) in laid.lines.iter().enumerate().skip(scroll).take(viewport) {
            let y = inner.y + (offset - scroll) as u16;
            let line = if shift > 0 {
                crop(line, shift)
            } else {
                line.clone()
            };
            frame.render_widget(
                Paragraph::new(line).style(theme::text()),
                Rect::new(inner.x, y, width, 1),
            );
        }
        let mut drew_images = false;
        for image in &laid.images {
            let top = image.y;
            let bottom = image.y + usize::from(image.height);
            if bottom <= scroll || top >= scroll + viewport {
                continue;
            }
            // Pictures move with sideways scrolling and are only drawn whole.
            let Some(left) = image.x.checked_sub(self.hscroll) else {
                continue;
            };
            if left >= width {
                continue;
            }
            let x = inner.x + left;
            let w = image.width.min(width - left);
            let whole = w == image.width;
            if top >= scroll && bottom <= scroll + viewport && whole {
                if let Some(protocol) = self
                    .body
                    .as_ref()
                    .and_then(|body| body.protocol(image.slot))
                {
                    frame.render_widget(
                        Image::new(protocol),
                        Rect::new(x, inner.y + (top - scroll) as u16, w, image.height),
                    );
                    drew_images = true;
                }
            } else {
                // Part of a picture scrolled out of view: shade what is left of it.
                let first = top.max(scroll);
                let last = bottom.min(scroll + viewport);
                for row in first..last {
                    frame.render_widget(
                        cut_image(usize::from(w)),
                        Rect::new(x, inner.y + (row - scroll) as u16, w, 1),
                    );
                }
            }
        }
        self.drew_images = drew_images;

        if self.max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            let mut state = ScrollbarState::new(laid.lines.len())
                .position(scroll)
                .viewport_content_length(viewport);
            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut state,
            );
        }
        if self.max_hscroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .begin_symbol(Some("◀"))
                .end_symbol(Some("▶"));
            let mut state = ScrollbarState::new(usize::from(self.max_hscroll) + usize::from(width))
                .position(shift)
                .viewport_content_length(usize::from(width));
            frame.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 0,
                    horizontal: 1,
                }),
                &mut state,
            );
        }
        if clear && area.width > 2 && area.height > 2 {
            // The corner handle that resizes the window.
            frame.render_widget(
                Paragraph::new("◢").style(theme::title(true)),
                Rect::new(area.right() - 1, area.bottom() - 1, 1, 1),
            );
        }
    }

    fn layout(&mut self, mail: &Mail, width: u16, picker: Option<&Picker>) -> Laid {
        let body = self.body(mail);
        let blocked = body.has_blocked();
        let mut lines = header_lines(mail, blocked);
        if let Some(kind) = mail.security {
            let status = self.security.clone().unwrap_or_else(|| {
                if kind.needs_secret() {
                    "encrypted · opening…".into()
                } else {
                    "signed · checking…".into()
                }
            });
            let color = if status.contains("BAD") || status.starts_with('⚠') {
                theme::error_color()
            } else if status.starts_with("signed by") && !status.contains("not verified")
                || status.contains("· signed by") && !status.contains("not verified")
            {
                theme::success_color()
            } else {
                theme::accent()
            };
            let position = lines.len() - mail.attachments.len();
            lines.insert(
                position,
                Line::from(vec![
                    Span::styled(format!("{:<8}", "security"), theme::muted()),
                    Span::styled(
                        status,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ]),
            );
        }
        if mail.partial {
            let files = lines.len() - mail.attachments.len();
            lines.insert(
                files,
                header_line("size", "headers only · D to download the rest", true),
            );
        }
        if mail.unsubscribe.is_some() {
            let files = lines.len() - mail.attachments.len();
            lines.insert(
                files,
                header_line("list", "mailing list · U to unsubscribe", false),
            );
        }
        let first_file = lines.len() - mail.attachments.len();
        let mut targets = Vec::new();
        let mut laid = Laid::default();
        for (index, line) in lines.into_iter().enumerate() {
            let start = laid.lines.len();
            laid.lines.extend(wrap(line, width));
            if index >= first_file {
                for row in start..laid.lines.len() {
                    targets.push((row, Target::Attachment(index - first_file)));
                }
            }
        }
        laid.lines.push(Line::default());
        let top = laid.lines.len();
        let body = self.body(mail);
        let html = body.is_html();
        let page = body.layout(width, picker);
        laid.width = page.width.max(width);
        laid.lines.extend(page.lines);
        laid.images = page
            .images
            .into_iter()
            .map(|image| Placed {
                y: image.y + top,
                ..image
            })
            .collect();
        laid.links = page
            .links
            .into_iter()
            .map(|link| LinkArea {
                y: link.y + top,
                ..link
            })
            .collect();
        // Plain-text mail lists its links below; HTML links are clickable in place.
        if !mail.links.is_empty() && !html {
            laid.lines.push(Line::default());
            laid.lines.push(Line::styled("links", theme::muted()));
            for (index, link) in mail.links.iter().enumerate() {
                let start = laid.lines.len();
                laid.lines.extend(wrap(
                    Line::styled(
                        format!("  {}. {link}", index + 1),
                        theme::text().add_modifier(Modifier::UNDERLINED),
                    ),
                    width,
                ));
                for row in start..laid.lines.len() {
                    targets.push((row, Target::Link(link.clone())));
                }
            }
        }
        self.targets = targets;
        laid
    }
}

/// The part of a line from column `start` on.
fn crop(line: &Line<'static>, start: usize) -> Line<'static> {
    let mut skipped = 0;
    let mut spans = Vec::new();
    for span in &line.spans {
        if skipped >= start {
            spans.push(span.clone());
            continue;
        }
        let mut kept = String::new();
        for character in span.content.chars() {
            if skipped >= start {
                kept.push(character);
            } else {
                skipped += unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
                // A wide character cut in half leaves a space behind.
                if skipped > start {
                    kept.push(' ');
                }
            }
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, span.style));
        }
    }
    Line::from(spans).style(line.style)
}

fn cut_image(width: usize) -> Paragraph<'static> {
    Paragraph::new("░".repeat(width.max(1))).style(theme::muted())
}

fn header_lines(mail: &Mail, blocked: bool) -> Vec<Line<'static>> {
    let mut lines = vec![header_line("from", &mail.from, true)];
    if !mail.to.is_empty() {
        lines.push(header_line("to", &mail.to.join(", "), false));
    }
    if !mail.cc.is_empty() {
        lines.push(header_line("cc", &mail.cc.join(", "), false));
    }
    if let Some(timestamp) = mail.timestamp.filter(|_| mail.date.is_some()) {
        lines.push(header_line(
            "date",
            &crate::mail::long_date(timestamp),
            false,
        ));
    }
    lines.push(header_line("subject", &mail.subject, true));
    if let Some(status) = &mail.delivery_status {
        lines.push(header_line("delivery", status, true));
    }
    if mail.html {
        lines.push(header_line(
            "format",
            if blocked {
                "html · remote images blocked · I to load"
            } else {
                "html"
            },
            false,
        ));
    }
    for (index, attachment) in mail.attachments.iter().enumerate() {
        lines.push(header_line(
            if index == 0 { "files" } else { "" },
            &format!(
                "{}. {} · {}",
                index + 1,
                attachment.name,
                size_label(attachment.size)
            ),
            false,
        ));
    }
    lines
}

fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn header_line(label: &str, value: &str, bold: bool) -> Line<'static> {
    let style = if bold {
        Style::default()
            .fg(theme::text_color())
            .add_modifier(Modifier::BOLD)
    } else {
        theme::text()
    };
    Line::from(vec![
        Span::styled(format!("{label:<8}"), theme::muted()),
        Span::styled(value.to_owned(), style),
    ])
}

#[cfg(test)]
mod tests {
    use crate::mail::{Attachment, demo_mails};
    use ratatui::{Terminal, backend::TestBackend};

    use super::Reader;

    #[test]
    fn keeps_scroll_in_bounds() {
        let mut reader = Reader::new("mail-1".into());
        reader.max_scroll = 3;

        reader.scroll_down(5);
        assert_eq!(reader.scroll, 3);
        reader.scroll_up(2);
        assert_eq!(reader.scroll, 1);
    }

    #[test]
    fn renders_docked_message_content() {
        let mut reader = Reader::new("project-1".into());
        let mail = &demo_mails()[0];
        let backend = TestBackend::new(44, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| reader.draw_docked(frame, frame.area(), mail, None))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(screen.contains("message"));
        assert!(screen.contains("ada@example.com"));
        assert!(screen.contains("the new build is ready"));
    }

    #[test]
    fn renders_safe_rich_message_summary() {
        let mut reader = Reader::new("project-1".into());
        let mut mail = demo_mails().remove(0);
        mail.html = true;
        mail.links = vec!["https://example.com/report".into()];
        mail.attachments = vec![Attachment {
            name: "report.pdf".into(),
            media_type: "application/pdf".into(),
            size: 2048,
        }];
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| reader.draw_docked(frame, frame.area(), &mail, None))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(screen.contains("html"));
        assert!(screen.contains("report.pdf"));
    }
}

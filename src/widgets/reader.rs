use crate::{
    mail::Mail,
    theme,
    widgets::rich::{Body, Row, wrap},
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

pub struct Reader {
    mail_id: String,
    scroll: u16,
    max_scroll: u16,
    body: Option<Body>,
    /// Laid-out rows and the width they were laid out for.
    rows: Option<(u16, Vec<Row>)>,
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
        self.rows = None;
    }

    pub fn set_security(&mut self, status: String) {
        self.security = Some(status);
        self.rows = None;
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
        self.rows = None;
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, mail: &Mail, picker: Option<&Picker>) {
        let area = reader_area(area);
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::border(true))
            .title_style(theme::title(true))
            .title(" message ")
            .bg(theme::surface());
        let inner = block.inner(area);
        self.frame_area = area;
        self.text_area = inner;
        if clear {
            frame.render_widget(Clear, area);
        }
        frame.render_widget(block, area);
        let width = inner.width;
        if self
            .rows
            .as_ref()
            .is_none_or(|(laid_out, _)| *laid_out != width)
        {
            let rows = self.layout(mail, width, picker);
            self.rows = Some((width, rows));
        }
        let Some((_, rows)) = &self.rows else {
            return;
        };
        let viewport = usize::from(inner.height);
        self.max_scroll = rows.len().saturating_sub(viewport) as u16;
        self.scroll = self.scroll.min(self.max_scroll);
        let scroll = usize::from(self.scroll);
        let mut drew_images = false;
        for (offset, row) in rows.iter().enumerate().skip(scroll).take(viewport) {
            let y = inner.y + (offset - scroll) as u16;
            let line_area = Rect::new(inner.x, y, width, 1);
            match row {
                Row::Line(line) => frame
                    .render_widget(Paragraph::new(line.clone()).style(theme::text()), line_area),
                Row::Image { slot, height } => {
                    let fits = offset - scroll + usize::from(*height) <= viewport;
                    match self.body.as_ref().and_then(|body| body.protocol(*slot)) {
                        Some(protocol) if fits => {
                            frame.render_widget(
                                Image::new(protocol),
                                Rect::new(inner.x, y, width, *height),
                            );
                            drew_images = true;
                        }
                        _ => frame.render_widget(cut_image(), line_area),
                    }
                }
                // Part of a picture that is scrolled half out of view.
                Row::Covered { start } if *start < scroll => {
                    frame.render_widget(cut_image(), line_area);
                }
                Row::Covered { .. } => {}
            }
        }
        self.drew_images = drew_images;

        if self.max_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            let mut state = ScrollbarState::new(rows.len())
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
    }

    fn layout(&mut self, mail: &Mail, width: u16, picker: Option<&Picker>) -> Vec<Row> {
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
        let mut rows: Vec<Row> = Vec::new();
        for (index, line) in lines.into_iter().enumerate() {
            let start = rows.len();
            rows.extend(wrap(line, width).into_iter().map(Row::Line));
            if index >= first_file {
                for row in start..rows.len() {
                    targets.push((row, Target::Attachment(index - first_file)));
                }
            }
        }
        rows.push(Row::Line(Line::default()));
        rows.extend(self.body(mail).rows(width, picker));
        if !mail.links.is_empty() {
            rows.push(Row::Line(Line::default()));
            rows.push(Row::Line(Line::styled("links", theme::muted())));
            for (index, link) in mail.links.iter().enumerate() {
                let start = rows.len();
                rows.extend(
                    wrap(
                        Line::styled(
                            format!("  {}. {link}", index + 1),
                            theme::text().add_modifier(Modifier::UNDERLINED),
                        ),
                        width,
                    )
                    .into_iter()
                    .map(Row::Line),
                );
                for row in start..rows.len() {
                    targets.push((row, Target::Link(link.clone())));
                }
            }
        }
        self.targets = targets;
        rows
    }
}

fn cut_image() -> Paragraph<'static> {
    Paragraph::new("░".repeat(4)).style(theme::muted())
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

fn reader_area(area: Rect) -> Rect {
    // Wide enough for the 600-pixel layouts most HTML mail is designed for.
    let width = area.width.saturating_sub(4).min(100);
    let height = area.height.saturating_sub(4);

    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
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
        assert!(screen.contains("https://example.com/report"));
    }
}

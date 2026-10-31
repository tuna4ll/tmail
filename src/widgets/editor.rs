//! A small multi-line editor for message bodies, for when no external editor is wanted.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::theme;

#[derive(Debug, Default)]
pub struct Editor {
    lines: Vec<String>,
    /// Cursor line and character index within it.
    row: usize,
    column: usize,
    /// First visible line and the height last drawn.
    top: usize,
    height: usize,
    /// Where the text was drawn, for mouse clicks.
    area: Rect,
    pub modified: bool,
}

/// What a key did.
pub enum Outcome {
    Edited,
    /// Leave the editor and keep the text.
    Done,
    Ignored,
}

impl Editor {
    pub fn new(text: &str) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            ..Self::default()
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn byte(&self, row: usize, column: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(column)
            .map_or(self.lines[row].len(), |(byte, _)| byte)
    }

    fn clamp(&mut self) {
        self.row = self.row.min(self.lines.len() - 1);
        self.column = self.column.min(self.line_len(self.row));
    }

    pub fn insert(&mut self, text: &str) {
        for (index, part) in text.replace("\r\n", "\n").split('\n').enumerate() {
            if index > 0 {
                self.newline();
            }
            let clean: String = part
                .chars()
                .map(|character| if character == '\t' { ' ' } else { character })
                .filter(|character| !character.is_control())
                .collect();
            let byte = self.byte(self.row, self.column);
            self.lines[self.row].insert_str(byte, &clean);
            self.column += clean.chars().count();
        }
        self.modified = true;
    }

    fn newline(&mut self) {
        let byte = self.byte(self.row, self.column);
        let rest = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.column = 0;
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            let start = self.byte(self.row, self.column - 1);
            let end = self.byte(self.row, self.column);
            self.lines[self.row].replace_range(start..end, "");
            self.column -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.column = self.line_len(self.row);
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.column < self.line_len(self.row) {
            let start = self.byte(self.row, self.column);
            let end = self.byte(self.row, self.column + 1);
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn word_left(&mut self) {
        let characters: Vec<char> = self.lines[self.row].chars().collect();
        if self.column == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.column = self.line_len(self.row);
            }
            return;
        }
        let mut index = self.column;
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !characters[index - 1].is_whitespace() {
            index -= 1;
        }
        self.column = index;
    }

    fn word_right(&mut self) {
        let characters: Vec<char> = self.lines[self.row].chars().collect();
        if self.column >= characters.len() {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.column = 0;
            }
            return;
        }
        let mut index = self.column;
        while index < characters.len() && characters[index].is_whitespace() {
            index += 1;
        }
        while index < characters.len() && !characters[index].is_whitespace() {
            index += 1;
        }
        self.column = index;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let edited = match key.code {
            KeyCode::Esc => return Outcome::Done,
            KeyCode::Char('s' | 'S') if control => return Outcome::Done,
            KeyCode::Char(character) if !control && !alt => {
                self.insert(&character.to_string());
                true
            }
            KeyCode::Enter => {
                self.newline();
                true
            }
            KeyCode::Tab => {
                self.insert("    ");
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            KeyCode::Char('k' | 'K') if control => {
                if self.lines.len() > 1 {
                    self.lines.remove(self.row);
                } else {
                    self.lines[0].clear();
                }
                self.clamp();
                true
            }
            KeyCode::Char('w' | 'W') if control => {
                let end = self.column;
                self.word_left();
                if self.row < self.lines.len() && self.column < end {
                    let start = self.byte(self.row, self.column);
                    let stop = self.byte(self.row, end);
                    self.lines[self.row].replace_range(start..stop, "");
                }
                true
            }
            KeyCode::Left if control || alt => {
                self.word_left();
                false
            }
            KeyCode::Right if control || alt => {
                self.word_right();
                false
            }
            KeyCode::Left => {
                if self.column > 0 {
                    self.column -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.column = self.line_len(self.row);
                }
                false
            }
            KeyCode::Right => {
                if self.column < self.line_len(self.row) {
                    self.column += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.column = 0;
                }
                false
            }
            KeyCode::Up => {
                self.row = self.row.saturating_sub(1);
                self.clamp();
                false
            }
            KeyCode::Down => {
                self.row = (self.row + 1).min(self.lines.len() - 1);
                self.clamp();
                false
            }
            KeyCode::Home => {
                self.column = 0;
                false
            }
            KeyCode::Char('a' | 'A') if control => {
                self.column = 0;
                false
            }
            KeyCode::End => {
                self.column = self.line_len(self.row);
                false
            }
            KeyCode::Char('e' | 'E') if control => {
                self.column = self.line_len(self.row);
                false
            }
            KeyCode::PageUp => {
                self.row = self.row.saturating_sub(self.height.max(1));
                self.clamp();
                false
            }
            KeyCode::PageDown => {
                self.row = (self.row + self.height.max(1)).min(self.lines.len() - 1);
                self.clamp();
                false
            }
            _ => return Outcome::Ignored,
        };
        if edited {
            self.modified = true;
        }
        Outcome::Edited
    }

    /// Scrolls without moving the cursor off screen.
    pub fn scroll(&mut self, down: bool, amount: usize) {
        let last = self.lines.len().saturating_sub(1);
        self.top = if down {
            (self.top + amount).min(last)
        } else {
            self.top.saturating_sub(amount)
        };
        if self.row < self.top {
            self.row = self.top;
        } else if self.height > 0 && self.row >= self.top + self.height {
            self.row = self.top + self.height - 1;
        }
        self.clamp();
    }

    /// Moves the cursor to a clicked cell.
    pub fn click(&mut self, column: u16, row: u16) -> bool {
        let area = self.area;
        if column < area.x || row < area.y || column >= area.right() || row >= area.bottom() {
            return false;
        }
        self.row = (self.top + usize::from(row - area.y)).min(self.lines.len() - 1);
        let target = usize::from(column - area.x);
        let mut width = 0;
        self.column = self.line_len(self.row);
        for (index, character) in self.lines[self.row].chars().enumerate() {
            let next = width + UnicodeWidthStr::width(character.to_string().as_str());
            if next > target {
                self.column = index;
                break;
            }
            width = next;
        }
        true
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, title: &str) {
        let block = Block::bordered()
            .border_style(theme::border(true))
            .title_style(theme::title(true))
            .title(title.to_owned())
            .title_bottom(Line::styled(
                " ctrl+s or esc  done · ctrl+k  delete line · ctrl+w  delete word ",
                theme::muted(),
            ))
            .style(Style::default().bg(theme::surface()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.area = inner;
        self.height = usize::from(inner.height);
        if self.row < self.top {
            self.top = self.row;
        } else if self.height > 0 && self.row >= self.top + self.height {
            self.top = self.row + 1 - self.height;
        }
        // Long lines scroll sideways around the cursor instead of wrapping, so a
        // screen row is always one line of the message.
        let before: String = self.lines[self.row].chars().take(self.column).collect();
        let cursor_x = UnicodeWidthStr::width(before.as_str());
        let width = usize::from(inner.width.max(1));
        let shift = cursor_x.saturating_sub(width.saturating_sub(1));
        let lines: Vec<Line> = self
            .lines
            .iter()
            .skip(self.top)
            .take(self.height)
            .map(|line| {
                let visible: String = if shift > 0 {
                    let mut skipped = 0;
                    line.chars()
                        .skip_while(|character| {
                            let width = UnicodeWidthStr::width(character.to_string().as_str());
                            let skip = skipped < shift;
                            skipped += width;
                            skip
                        })
                        .collect()
                } else {
                    line.clone()
                };
                let style = if line.starts_with('>') {
                    theme::muted()
                } else {
                    theme::text()
                };
                Line::from(Span::styled(visible, style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        let x = inner.x + u16::try_from(cursor_x - shift).unwrap_or(0);
        let y = inner.y + u16::try_from(self.row - self.top).unwrap_or(0);
        frame.set_cursor_position(Position::new(x.min(inner.right().saturating_sub(1)), y));
    }
}

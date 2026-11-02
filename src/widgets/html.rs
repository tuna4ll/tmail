//! A small layout engine that draws HTML mail on a grid of terminal cells.
//!
//! It understands the parts of HTML and CSS that mail actually uses: `<style>` rules
//! and inline styles, background and text colours, padding, margins, borders, widths,
//! centring, tables laid out by column, lists, quotes, buttons and pictures. Sizes in
//! pixels are turned into cells at roughly eight pixels per column and sixteen per row.

use std::collections::HashMap;

use ego_tree::NodeId;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use scraper::{ElementRef, Html, Node, Selector};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Pixels per terminal column and row.
const PX_COLUMN: f32 = 8.0;
const PX_ROW: f32 = 16.0;
/// Deeper nesting than this is drawn as plain text, so hostile mail cannot exhaust
/// the stack.
const MAX_DEPTH: usize = 120;

pub const IMAGE_START: char = '\u{E000}';
pub const IMAGE_END: char = '\u{E001}';

pub type Rgb = (u8, u8, u8);

/// How a page is drawn.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Uses the message's own colours, as a browser would. Otherwise the page takes
    /// the reader's colours and keeps only accents and coloured blocks.
    pub original: bool,
    /// The reader's background and text, for the themed mode.
    pub surface: Rgb,
    pub text: Rgb,
    pub link: Rgb,
}

/// A picture placed on the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub slot: usize,
    pub x: u16,
    pub y: usize,
    pub width: u16,
    pub height: u16,
}

/// A link's cells on one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkArea {
    pub y: usize,
    pub start: u16,
    pub end: u16,
    pub url: String,
}

/// The widest a page is laid out when it does not fit the window; wider pages are
/// squeezed instead of scrolled.
const MAX_PAGE_WIDTH: usize = 240;

#[derive(Default)]
pub struct Page {
    /// The width the page was laid out at. More than asked for when its content has a
    /// fixed width, such as a 600 pixel newsletter in a narrow window.
    pub width: u16,
    pub lines: Vec<Line<'static>>,
    pub images: Vec<Placed>,
    pub links: Vec<LinkArea>,
}

/// How big picture `slot` should be, in cells, when at most `columns` wide. An error
/// gives the text to show instead, if any.
pub type ImageSize<'a> = dyn FnMut(usize, u16) -> Result<(u16, u16), Option<String>> + 'a;

/// A parsed message, kept while it is open so a new width only redoes the layout:
/// the tree, the CSS each element gets, and how wide each element wants to be.
pub struct Document {
    html: Html,
    /// Declarations from `<style>` rules in cascade order, then the inline style.
    declarations: HashMap<NodeId, Vec<(String, String)>>,
    measures: std::cell::RefCell<HashMap<NodeId, (usize, usize)>>,
}

impl Document {
    pub fn parse(source: &str) -> Self {
        let html = Html::parse_document(source);
        let mut declarations: HashMap<NodeId, Vec<(String, String)>> = collect_rules(&html)
            .into_iter()
            .map(|(id, rules)| {
                let list = rules
                    .into_iter()
                    .flat_map(|rule| rule.declarations)
                    .collect();
                (id, list)
            })
            .collect();
        for node in html.tree.nodes() {
            if let Some(element) = ElementRef::wrap(node)
                && let Some(inline) = element.value().attr("style")
            {
                declarations
                    .entry(node.id())
                    .or_default()
                    .extend(self::declarations(inline));
            }
        }
        Self {
            html,
            declarations,
            measures: Default::default(),
        }
    }

    /// Forgets element widths, after a picture arrived and changed its size.
    pub fn forget_measures(&self) {
        self.measures.borrow_mut().clear();
    }
}

pub fn render_document(
    document: &Document,
    width: u16,
    options: Options,
    image_size: &mut ImageSize<'_>,
) -> Page {
    let mut engine = Engine::new(document, options, image_size);
    let root = document.html.root_element();
    let style = engine.root_style.clone();
    let (narrowest, _) = engine.measure(root, &style, 0);
    let width = usize::from(width.max(10)).max(narrowest.min(MAX_PAGE_WIDTH));
    let mut canvas = Canvas::new(width, engine.page_fill);
    engine.block_children(root, &style, width, &mut canvas, 0);
    canvas.trim_trailing_blank();
    let mut page = canvas.into_page();
    page.width = width as u16;
    page
}

// ----------------------------------------------------------------------------
// Styles

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Display {
    Block,
    Inline,
    InlineBlock,
    ListItem,
    Table,
    Row,
    Cell,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Length {
    Cells(f32),
    Percent(f32),
}

impl Length {
    fn resolve(self, available: usize) -> usize {
        match self {
            Self::Cells(cells) => cells.round().max(0.0) as usize,
            Self::Percent(percent) => ((available as f32) * percent / 100.0).round() as usize,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Sides {
    top: usize,
    right: usize,
    bottom: usize,
    left: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct Border {
    top: bool,
    right: bool,
    bottom: bool,
    left: bool,
    color: Option<Rgb>,
}

impl Border {
    fn any(&self) -> bool {
        self.top || self.right || self.bottom || self.left
    }
}

#[derive(Clone, Debug)]
struct Computed {
    display: Display,
    // Inherited.
    color: Option<Rgb>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    align: Align,
    pre: bool,
    uppercase: bool,
    hidden: bool,
    link: Option<String>,
    // Not inherited.
    background: Option<Rgb>,
    padding: Sides,
    margin: Sides,
    border: Border,
    width: Option<Length>,
    max_width: Option<Length>,
    center: bool,
    float_right: bool,
    /// Table cells sit in the middle of their row unless told otherwise.
    valign_top: bool,
    /// A table with `border="1"` draws lines between its cells.
    grid: bool,
    /// `align="center"` and `<center>` also centre the tables inside, as browsers do.
    center_children: bool,
}

impl Computed {
    fn root() -> Self {
        Self {
            display: Display::Block,
            color: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            align: Align::Left,
            pre: false,
            uppercase: false,
            hidden: false,
            link: None,
            background: None,
            padding: Sides::default(),
            margin: Sides::default(),
            border: Border::default(),
            width: None,
            max_width: None,
            center: false,
            float_right: false,
            valign_top: false,
            grid: false,
            center_children: false,
        }
    }

    fn child(&self) -> Self {
        Self {
            display: Display::Inline,
            background: None,
            padding: Sides::default(),
            margin: Sides::default(),
            border: Border::default(),
            width: None,
            max_width: None,
            center: false,
            float_right: false,
            valign_top: false,
            grid: false,
            ..self.clone()
        }
    }

    fn is_block_level(&self) -> bool {
        matches!(
            self.display,
            Display::Block | Display::ListItem | Display::Table | Display::Row | Display::Cell
        )
    }
}

/// A table row: the row, its style, and each cell with its style and column span.
type TableRow<'c> = (
    ElementRef<'c>,
    Computed,
    Vec<(ElementRef<'c>, Computed, usize)>,
);

struct Rule {
    order: usize,
    specificity: u32,
    declarations: Vec<(String, String)>,
}

// ----------------------------------------------------------------------------
// The canvas: rows of styled cells exactly `width` wide.

/// One terminal cell. Wide characters fill the cells after them with `'\0'`, and
/// combining marks ride along in `extra`. Kept small: a page has many of these.
#[derive(Clone)]
struct Cell {
    ch: char,
    extra: Option<Box<str>>,
    style: Style,
    link: Option<usize>,
}

impl Cell {
    fn blank(&self) -> bool {
        (self.ch == ' ' || self.ch == '\0') && self.extra.is_none()
    }

    fn width(&self) -> usize {
        if self.ch == '\0' {
            0
        } else {
            self.ch.width().unwrap_or(0)
        }
    }
}

struct Canvas {
    width: usize,
    fill: Style,
    rows: Vec<Vec<Cell>>,
    images: Vec<Placed>,
    links: Vec<String>,
    /// Blank rows owed before the next content, from a bottom margin.
    pending: usize,
}

impl Canvas {
    fn new(width: usize, fill: Style) -> Self {
        Self {
            width,
            fill,
            rows: Vec::new(),
            images: Vec::new(),
            links: Vec::new(),
            pending: 0,
        }
    }

    fn blank_row(&self, fill: Style) -> Vec<Cell> {
        vec![
            Cell {
                ch: ' ',
                extra: None,
                style: fill,
                link: None,
            };
            self.width
        ]
    }

    fn push_blank(&mut self, fill: Style) {
        let row = self.blank_row(fill);
        self.rows.push(row);
    }

    fn trailing_blank(&self) -> usize {
        self.rows
            .iter()
            .rev()
            .take_while(|row| row.iter().all(Cell::blank))
            .count()
    }

    /// Adds vertical space, collapsing with blank rows already there.
    fn margin(&mut self, rows: usize) {
        let owed = rows.max(self.pending);
        self.pending = 0;
        if self.rows.is_empty() {
            return;
        }
        let have = self.trailing_blank();
        let fill = self.fill;
        for _ in have..owed {
            self.push_blank(fill);
        }
    }

    fn trim_trailing_blank(&mut self) {
        let blank = self.trailing_blank();
        let keep = self.rows.len() - blank;
        let images_below = self
            .images
            .iter()
            .any(|image| image.y + usize::from(image.height) > keep);
        if !images_below {
            self.rows.truncate(keep);
        }
    }

    fn link(&mut self, url: &str) -> usize {
        if let Some(index) = self.links.iter().position(|known| known == url) {
            return index;
        }
        self.links.push(url.to_owned());
        self.links.len() - 1
    }

    /// Copies `other` onto this canvas at column `x`, below the current rows when
    /// `y` is `None`.
    fn draw(&mut self, other: Canvas, x: usize) {
        let y = self.rows.len();
        let fill = self.fill;
        let mapping: Vec<usize> = other.links.iter().map(|url| self.link(url)).collect();
        for row in other.rows {
            let mut line = self.blank_row(fill);
            for (offset, mut cell) in row.into_iter().enumerate() {
                if x + offset < self.width {
                    cell.link = cell.link.map(|index| mapping[index]);
                    line[x + offset] = cell;
                }
            }
            self.rows.push(line);
        }
        for mut image in other.images {
            image.x += x as u16;
            image.y += y;
            self.images.push(image);
        }
    }

    fn into_page(self) -> Page {
        let mut page = Page {
            images: self.images,
            ..Page::default()
        };
        for (y, row) in self.rows.into_iter().enumerate() {
            let row = whole_characters(row);
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut column = 0u16;
            let mut open: Option<(usize, u16)> = None;
            for cell in row {
                let width = cell.width() as u16;
                match (open, cell.link) {
                    (Some((index, start)), Some(current)) if index == current => {
                        let _ = start;
                    }
                    (Some((index, start)), _) => {
                        page.links.push(LinkArea {
                            y,
                            start,
                            end: column,
                            url: self.links[index].clone(),
                        });
                        open = cell.link.map(|index| (index, column));
                    }
                    (None, Some(index)) => open = Some((index, column)),
                    (None, None) => {}
                }
                column += width;
                if width == 0 {
                    continue;
                }
                let mut text = String::new();
                text.push(cell.ch);
                if let Some(extra) = &cell.extra {
                    text.push_str(extra);
                }
                match spans.last_mut() {
                    Some(last) if last.style == cell.style => {
                        last.content.to_mut().push_str(&text);
                    }
                    _ => spans.push(Span::styled(text, cell.style)),
                }
            }
            if let Some((index, start)) = open {
                page.links.push(LinkArea {
                    y,
                    start,
                    end: column,
                    url: self.links[index].clone(),
                });
            }
            page.lines.push(Line::from(spans));
        }
        page
    }
}

/// Repairs wide characters that something else drew over: a half without its other
/// half becomes a space, so every row keeps its width.
fn whole_characters(row: Vec<Cell>) -> Vec<Cell> {
    let blank = |cell: &Cell| Cell {
        ch: ' ',
        extra: None,
        style: cell.style,
        link: cell.link,
    };
    let mut fixed = Vec::with_capacity(row.len());
    let mut index = 0;
    while index < row.len() {
        let cell = &row[index];
        let whole_wide =
            cell.width() == 2 && row.get(index + 1).is_some_and(|next| next.ch == '\0');
        if whole_wide {
            fixed.push(cell.clone());
            fixed.push(row[index + 1].clone());
            index += 2;
            continue;
        }
        fixed.push(if cell.width() == 1 {
            cell.clone()
        } else {
            blank(cell)
        });
        index += 1;
    }
    fixed
}

// ----------------------------------------------------------------------------
// Inline content

enum Piece {
    /// One word with the spaces after it.
    Word {
        text: String,
        style: Style,
        link: Option<String>,
        space: bool,
    },
    Break,
    /// Something drawn as a box inside a line: a picture or an inline block.
    Atom {
        width: usize,
        height: usize,
        content: Atom,
        link: Option<String>,
    },
}

enum Atom {
    Image(usize),
    Box(Canvas),
}

struct Engine<'a, 'b> {
    declarations: &'a HashMap<NodeId, Vec<(String, String)>>,
    measures: &'a std::cell::RefCell<HashMap<NodeId, (usize, usize)>>,
    options: Options,
    image_size: &'a mut ImageSize<'b>,
    page_fill: Style,
    root_style: Computed,
    list_counters: Vec<usize>,
}

impl<'a, 'b> Engine<'a, 'b> {
    fn new(source: &'a Document, options: Options, image_size: &'a mut ImageSize<'b>) -> Self {
        let document = &source.html;
        let mut engine = Self {
            declarations: &source.declarations,
            measures: &source.measures,
            options,
            image_size,
            page_fill: Style::default(),
            root_style: Computed::root(),
            list_counters: Vec::new(),
        };
        // The page colour comes from <body> (or <html>), the way a browser paints it.
        let root = document.root_element();
        let mut root_style = engine.compute(root, &Computed::root());
        let body = root
            .child_elements()
            .find(|element| element.value().name() == "body");
        if let Some(body) = body {
            let body_style = engine.compute(body, &root_style);
            if root_style.background.is_none() {
                root_style.background = body_style.background;
            }
            if root_style.color.is_none() {
                root_style.color = body_style.color;
            }
        }
        let background = if options.original {
            Some(root_style.background.unwrap_or((255, 255, 255)))
        } else {
            None
        };
        if options.original && root_style.color.is_none() {
            root_style.color = Some((34, 34, 34));
        }
        engine.page_fill = engine.fill(background);
        root_style.background = background;
        engine.root_style = root_style;
        engine
    }

    fn fill(&self, background: Option<Rgb>) -> Style {
        match background {
            Some((r, g, b)) => Style::default().bg(Color::Rgb(r, g, b)),
            None => Style::default(),
        }
    }

    // ---- styles ----

    fn compute(&self, element: ElementRef<'_>, parent: &Computed) -> Computed {
        let mut style = parent.child();
        let tag = element.value().name();
        self.defaults(tag, element, &mut style);
        self.attributes(tag, element, &mut style);
        if let Some(list) = self.declarations.get(&element.id()) {
            for (name, value) in list {
                apply(&mut style, name, value);
            }
        }
        if !self.options.original {
            self.theme(&mut style, parent);
        }
        style
    }

    /// Keeps a message readable in the reader's colours: page-like backgrounds go,
    /// coloured blocks and buttons stay, and text keeps enough contrast.
    fn theme(&self, style: &mut Computed, parent: &Computed) {
        if let Some(background) = style.background
            && !colourful(background)
        {
            style.background = None;
        }
        let behind = style
            .background
            .or(parent.background)
            .unwrap_or(self.options.surface);
        if let Some(color) = style.color
            && contrast(color, behind) < 3.0
        {
            style.color = None;
        }
        if style.background.is_some()
            && style.color.is_none()
            && contrast(self.options.text, behind) < 3.0
        {
            style.color = Some((20, 20, 20));
        }
    }

    fn defaults(&self, tag: &str, element: ElementRef<'_>, style: &mut Computed) {
        style.display = match tag {
            "html" | "body" | "div" | "p" | "section" | "article" | "header" | "footer"
            | "main" | "nav" | "aside" | "form" | "fieldset" | "address" | "figure"
            | "figcaption" | "center" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol"
            | "dl" | "dt" | "dd" | "blockquote" | "pre" | "hr" | "details" | "summary" => {
                Display::Block
            }
            "li" => Display::ListItem,
            "table" => Display::Table,
            "tr" => Display::Row,
            "td" | "th" => Display::Cell,
            "thead" | "tbody" | "tfoot" => Display::Block,
            "head" | "style" | "script" | "title" | "meta" | "link" | "noscript" | "template"
            | "base" | "colgroup" | "col" | "iframe" | "object" | "svg" => Display::None,
            "button" => Display::InlineBlock,
            _ => Display::Inline,
        };
        let row = |rows: usize| Sides {
            top: rows,
            bottom: rows,
            ..Sides::default()
        };
        if matches!(
            tag,
            "p" | "ul" | "ol" | "dl" | "pre" | "blockquote" | "figure" | "h1" | "h2" | "h3"
        ) {
            style.margin = row(1);
        }
        if matches!(
            tag,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "b" | "strong" | "th" | "dt" | "summary"
        ) {
            style.bold = true;
        }
        match tag {
            "i" | "em" | "cite" | "var" | "dfn" => style.italic = true,
            "u" | "ins" => style.underline = true,
            "s" | "strike" | "del" => style.strike = true,
            "center" => {
                style.align = Align::Center;
                style.center_children = true;
            }
            "th" => style.align = Align::Center,
            "pre" => style.pre = true,
            "a" => {
                if let Some(href) = element.value().attr("href") {
                    let href = href.trim();
                    if !href.is_empty() && !href.starts_with('#') {
                        style.link = Some(href.to_owned());
                        style.underline = true;
                        style.color = Some(if self.options.original {
                            (6, 69, 173)
                        } else {
                            self.options.link
                        });
                    }
                }
            }
            "mark" => style.background = Some((255, 235, 120)),
            "code" | "kbd" | "samp" | "tt" if self.options.original => {
                style.background = Some((240, 240, 240));
            }
            "blockquote" => {
                style.padding.left = 2;
                style.border.left = true;
                style.border.color = Some((180, 180, 180));
            }
            "ul" | "ol" | "dd" => style.padding.left = 3,
            _ => {}
        }
        if tag == "h1" {
            style.uppercase = false;
            style.underline = true;
        }
        if tag == "table" {
            // As in mail clients, a table starts its text at the left again, and sits
            // in the middle when its container was centred with `align`.
            style.align = Align::Left;
            style.center |= style.center_children;
            style.center_children = false;
        }
        if element.value().attr("hidden").is_some() {
            style.display = Display::None;
        }
    }

    fn attributes(&self, tag: &str, element: ElementRef<'_>, style: &mut Computed) {
        let attribute = |name: &str| element.value().attr(name).map(str::trim);
        if let Some(color) = attribute("bgcolor").and_then(parse_color) {
            style.background = Some(color);
        }
        if tag == "font"
            && let Some(color) = attribute("color").and_then(parse_color)
        {
            style.color = Some(color);
        }
        if let Some(width) = attribute("width").and_then(parse_attribute_length) {
            style.width = Some(width);
        }
        if matches!(
            attribute("valign").map(str::to_ascii_lowercase).as_deref(),
            Some("top" | "baseline")
        ) {
            style.valign_top = true;
        }
        match attribute("align").map(str::to_ascii_lowercase).as_deref() {
            Some("center") if matches!(tag, "table" | "img") => style.center = true,
            Some("center") => {
                style.align = Align::Center;
                style.center_children = true;
            }
            Some("right") if tag == "table" || tag == "img" => style.float_right = true,
            Some("right") => style.align = Align::Right,
            Some("left") => style.align = Align::Left,
            _ => {}
        }
        if tag == "table"
            && attribute("border")
                .and_then(|value| value.parse::<u32>().ok())
                .is_some_and(|value| value > 0)
        {
            style.border = Border {
                top: true,
                right: true,
                bottom: true,
                left: true,
                color: Some((150, 150, 150)),
            };
            style.grid = true;
        }
    }

    // ---- block layout ----

    fn block_children(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        width: usize,
        canvas: &mut Canvas,
        depth: usize,
    ) {
        let mut inline: Vec<Piece> = Vec::new();
        for child in element.children() {
            match child.value() {
                Node::Text(text) => {
                    self.text_pieces(text, style, &mut inline);
                }
                Node::Element(_) => {
                    let Some(child_element) = ElementRef::wrap(child) else {
                        continue;
                    };
                    let child_style = self.compute(child_element, style);
                    if child_style.display == Display::None || child_style.hidden {
                        continue;
                    }
                    if depth >= MAX_DEPTH {
                        // Too deep to lay out: show the rest as plain text, read without
                        // recursion so hostile mail cannot overflow the stack.
                        let text: String = child_element
                            .descendants()
                            .filter_map(|node| {
                                node.value().as_text().map(|text| format!("{} ", &**text))
                            })
                            .collect();
                        self.text_pieces(&text, style, &mut inline);
                    } else if child_style.is_block_level() {
                        self.flush_inline(&mut inline, style, width, canvas);
                        self.block(child_element, &child_style, width, canvas, depth + 1);
                    } else {
                        self.inline(child_element, &child_style, width, &mut inline, depth + 1);
                    }
                }
                _ => {}
            }
        }
        self.flush_inline(&mut inline, style, width, canvas);
    }

    fn fill_of(&self, style: &Computed, canvas: &Canvas) -> Style {
        match style.background {
            Some(background) => self.fill(Some(background)),
            None => canvas.fill,
        }
    }

    fn block(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        available: usize,
        canvas: &mut Canvas,
        depth: usize,
    ) {
        let tag = element.value().name();
        if tag == "hr" {
            canvas.margin(1);
            let color = style
                .border
                .color
                .or(style.color)
                .unwrap_or((170, 170, 170));
            let fill = canvas.fill;
            let mut row = canvas.blank_row(fill);
            for cell in &mut row {
                cell.ch = '─';
                cell.style = fill.fg(rgb(color));
            }
            canvas.rows.push(row);
            canvas.pending = 1;
            return;
        }
        let mut width = style
            .width
            .map_or(available, |width| width.resolve(available));
        if let Some(max) = style.max_width {
            width = width.min(max.resolve(available));
        }
        if style.display == Display::Table && style.width.is_none() {
            let (_, preferred) = self.measure(element, style, depth);
            width = width.min(preferred.max(1));
        }
        let width = width.clamp(1, available.max(1));
        let offset =
            if style.center || (style.align == Align::Center && style.display == Display::Table) {
                available.saturating_sub(width) / 2
            } else if style.float_right {
                available.saturating_sub(width)
            } else {
                0
            };
        canvas.margin(style.margin.top);
        let fill = self.fill_of(style, canvas);
        let border = style.border;
        let left = style.padding.left + usize::from(border.left);
        let right = style.padding.right + usize::from(border.right);
        let inner = width.saturating_sub(left + right).max(1);
        let mut content = Canvas::new(inner, fill);
        match style.display {
            Display::Table => self.table(element, style, inner, &mut content, depth),
            Display::ListItem => self.list_item(element, style, inner, &mut content, depth),
            _ => {
                let list = matches!(tag, "ul" | "ol");
                if list {
                    self.list_counters.push(0);
                }
                self.block_children(element, style, inner, &mut content, depth);
                if list {
                    self.list_counters.pop();
                }
            }
        }
        content.trim_trailing_blank();
        if content.rows.is_empty() && style.padding.top + style.padding.bottom == 0 && !border.any()
        {
            canvas.pending = canvas.pending.max(style.margin.bottom);
            return;
        }
        let framed = self.frame(content, style, width, fill);
        canvas.draw(framed, offset);
        canvas.pending = style.margin.bottom;
    }

    /// Wraps content in padding, background and borders.
    fn frame(&self, content: Canvas, style: &Computed, width: usize, fill: Style) -> Canvas {
        let border = style.border;
        let edge = fill.fg(rgb(border.color.unwrap_or((160, 160, 160))));
        let mut framed = Canvas::new(width, fill);
        let cell = |ch: char, style: Style| Cell {
            ch,
            extra: None,
            style,
            link: None,
        };
        let horizontal = |left: char, right: char| {
            let mut row = vec![cell('─', edge); width];
            if border.left {
                row[0] = cell(left, edge);
            }
            if border.right && width > 1 {
                row[width - 1] = cell(right, edge);
            }
            row
        };
        if border.top {
            framed.rows.push(horizontal('┌', '┐'));
        }
        for _ in 0..style.padding.top {
            framed.push_blank(fill);
        }
        let left = style.padding.left + usize::from(border.left);
        let start = framed.rows.len();
        framed.draw(content, left);
        if border.left || border.right {
            for row in &mut framed.rows[start..] {
                if border.left {
                    row[0] = cell('│', edge);
                }
                if border.right && width > 1 {
                    row[width - 1] = cell('│', edge);
                }
            }
        }
        for _ in 0..style.padding.bottom {
            let mut row = framed.blank_row(fill);
            if border.left {
                row[0] = cell('│', edge);
            }
            if border.right && width > 1 {
                row[width - 1] = cell('│', edge);
            }
            framed.rows.push(row);
        }
        if border.bottom {
            framed.rows.push(horizontal('└', '┘'));
        }
        framed
    }

    fn list_item(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        width: usize,
        canvas: &mut Canvas,
        depth: usize,
    ) {
        let ordered = element
            .parent()
            .and_then(ElementRef::wrap)
            .is_some_and(|parent| parent.value().name() == "ol");
        let marker = if ordered {
            let counter = self.list_counters.last_mut();
            let number = counter.map_or(1, |counter| {
                *counter += 1;
                *counter
            });
            format!("{number}. ")
        } else {
            let level = self.list_counters.len().saturating_sub(1);
            ["• ", "◦ ", "▪ "][level % 3].to_owned()
        };
        let indent = UnicodeWidthStr::width(marker.as_str());
        let mut content = Canvas::new(width.saturating_sub(indent).max(1), canvas.fill);
        self.block_children(element, style, content.width, &mut content, depth);
        content.trim_trailing_blank();
        let start = canvas.rows.len();
        canvas.draw(content, indent);
        if start == canvas.rows.len() {
            canvas.push_blank(canvas.fill);
        }
        let marker_style = canvas.fill.patch(self.text_style(style));
        for (offset, character) in marker.chars().enumerate().take(canvas.width) {
            canvas.rows[start][offset] = Cell {
                ch: character,
                extra: None,
                style: marker_style,
                link: None,
            };
        }
    }

    // ---- tables ----

    fn table_rows<'c>(&self, table: ElementRef<'c>, style: &Computed) -> Vec<TableRow<'c>> {
        let mut rows = Vec::new();
        let mut visit = |row: ElementRef<'c>, parent: &Computed| {
            let row_style = self.compute(row, parent);
            if row_style.display == Display::None {
                return;
            }
            let cells: Vec<_> = row
                .child_elements()
                .filter(|cell| matches!(cell.value().name(), "td" | "th"))
                .filter_map(|cell| {
                    let cell_style = self.compute(cell, &row_style);
                    (cell_style.display != Display::None).then(|| {
                        let span = cell
                            .value()
                            .attr("colspan")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(1)
                            .clamp(1, 50);
                        (cell, cell_style, span)
                    })
                })
                .collect();
            rows.push((row, row_style, cells));
        };
        for child in table.child_elements() {
            match child.value().name() {
                "tr" => visit(child, style),
                "thead" | "tbody" | "tfoot" => {
                    let group = self.compute(child, style);
                    for row in child
                        .child_elements()
                        .filter(|row| row.value().name() == "tr")
                    {
                        visit(row, &group);
                    }
                }
                _ => {}
            }
        }
        rows
    }

    fn cell_padding(&self, table: ElementRef<'_>) -> Sides {
        let pixels = table
            .value()
            .attr("cellpadding")
            .and_then(|value| value.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        let columns = (pixels / PX_COLUMN).round() as usize;
        let rows = (pixels / PX_ROW).round() as usize;
        Sides {
            top: rows,
            bottom: rows,
            left: columns.max(usize::from(pixels >= 4.0)),
            right: columns.max(usize::from(pixels >= 4.0)),
        }
    }

    fn table(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        width: usize,
        canvas: &mut Canvas,
        depth: usize,
    ) {
        let rows = self.table_rows(element, style);
        let columns = rows
            .iter()
            .map(|(_, _, cells)| cells.iter().map(|(_, _, span)| span).sum::<usize>())
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return;
        }
        let padding = self.cell_padding(element);
        let grid = style.grid;
        let separators = if grid { columns.saturating_sub(1) } else { 0 };
        let available = width.saturating_sub(separators).max(columns);
        // Column sizes: the narrowest a column can be and the width it would like.
        let mut minimum = vec![1usize; columns];
        let mut preferred = vec![1usize; columns];
        let mut fixed: Vec<Option<usize>> = vec![None; columns];
        for (_, _, cells) in &rows {
            let mut column = 0;
            for (cell, cell_style, span) in cells {
                let (low, high) = self.measure(*cell, cell_style, depth);
                let extra = padding.left
                    + padding.right
                    + cell_style.padding.left
                    + cell_style.padding.right;
                let (low, high) = (low + extra, high + extra);
                if *span == 1 && column < columns {
                    minimum[column] = minimum[column].max(low);
                    preferred[column] = preferred[column].max(high);
                    if let Some(width) = cell_style.width {
                        fixed[column] = Some(width.resolve(available).max(low));
                    }
                } else if column < columns {
                    let end = (column + span).min(columns);
                    let share = high.div_ceil(end - column);
                    for slot in &mut preferred[column..end] {
                        *slot = (*slot).max(share);
                    }
                }
                column += span;
            }
        }
        let mut widths: Vec<usize> = (0..columns)
            .map(|index| fixed[index].unwrap_or(preferred[index]))
            .collect();
        let total: usize = widths.iter().sum();
        if total > available {
            // Shrink the flexible columns toward their minimum, widest first.
            let mut excess = total - available;
            let mut guard = 0;
            while excess > 0 && guard < 10_000 {
                guard += 1;
                let Some(index) = (0..columns)
                    .filter(|index| widths[*index] > minimum[*index].min(available))
                    .max_by_key(|index| widths[*index].saturating_sub(minimum[*index]))
                else {
                    break;
                };
                widths[index] -= 1;
                excess -= 1;
            }
            // Still too wide: squeeze everything evenly.
            let total: usize = widths.iter().sum();
            if total > available {
                for width in &mut widths {
                    *width = (*width * available / total).max(1);
                }
            }
        } else if style.width.is_some() && total < available {
            // A table with a set width spreads the rest over columns without a width.
            let flexible: Vec<usize> = (0..columns)
                .filter(|index| fixed[*index].is_none())
                .collect();
            let targets = if flexible.is_empty() {
                (0..columns).collect()
            } else {
                flexible
            };
            let spare = available - total;
            for (position, index) in targets.iter().enumerate() {
                widths[*index] +=
                    spare / targets.len() + usize::from(position < spare % targets.len());
            }
        }
        let table_fill = canvas.fill;
        let edge = table_fill.fg(rgb(style.border.color.unwrap_or((160, 160, 160))));
        for (row_index, (_, row_style, cells)) in rows.iter().enumerate() {
            if grid && row_index > 0 {
                let mut line = canvas.blank_row(table_fill);
                for cell in line.iter_mut() {
                    *cell = Cell {
                        ch: '─',
                        extra: None,
                        style: edge,
                        link: None,
                    };
                }
                let mut x = 0;
                for (column, width) in widths.iter().enumerate() {
                    x += width;
                    if column + 1 < columns && x < line.len() {
                        line[x] = Cell {
                            ch: '┼',
                            extra: None,
                            style: edge,
                            link: None,
                        };
                        x += 1;
                    }
                }
                canvas.rows.push(line);
            }
            let row_fill = match row_style.background {
                Some(background) => self.fill(Some(background)),
                None => table_fill,
            };
            let mut rendered: Vec<(usize, Canvas, Style, &Computed)> = Vec::new();
            let mut column = 0;
            let mut x = 0;
            for (cell, cell_style, span) in cells {
                if column >= columns {
                    break;
                }
                let end = (column + span).min(columns);
                let cell_width = widths[column..end].iter().sum::<usize>()
                    + if grid { end - column - 1 } else { 0 };
                let fill = match cell_style.background {
                    Some(background) => self.fill(Some(background)),
                    None => row_fill,
                };
                let mut cell_box = cell_style.clone();
                cell_box.padding.left += padding.left;
                cell_box.padding.right += padding.right;
                cell_box.padding.top += padding.top;
                cell_box.padding.bottom += padding.bottom;
                cell_box.margin = Sides::default();
                cell_box.width = None;
                cell_box.max_width = None;
                let inner = cell_width
                    .saturating_sub(
                        cell_box.padding.left
                            + cell_box.padding.right
                            + usize::from(cell_box.border.left)
                            + usize::from(cell_box.border.right),
                    )
                    .max(1);
                let mut content = Canvas::new(inner, fill);
                self.block_children(*cell, &cell_box, inner, &mut content, depth + 1);
                content.trim_trailing_blank();
                let framed = self.frame(content, &cell_box, cell_width, fill);
                rendered.push((x, framed, fill, cell_style));
                x += cell_width + usize::from(grid);
                column = end;
            }
            let height = rendered
                .iter()
                .map(|(_, framed, ..)| framed.rows.len())
                .max()
                .unwrap_or(0)
                .max(1);
            let start = canvas.rows.len();
            for _ in 0..height {
                canvas.push_blank(row_fill);
            }
            for (x, mut framed, fill, cell_style) in rendered {
                let gap = height - framed.rows.len();
                let top = if cell_style.valign_top { 0 } else { gap / 2 };
                let width = framed.width;
                // Cells keep their own background down to the bottom of the row.
                for _ in 0..gap {
                    framed.rows.push(vec![
                        Cell {
                            ch: ' ',
                            extra: None,
                            style: fill,
                            link: None,
                        };
                        width
                    ]);
                }
                let links: Vec<usize> = framed.links.iter().map(|url| canvas.link(url)).collect();
                for (offset, row) in framed.rows.into_iter().enumerate() {
                    let target = &mut canvas.rows[start + top + offset.min(height - 1 - top)];
                    for (column, mut cell) in row.into_iter().enumerate() {
                        if x + column < target.len() {
                            cell.link = cell.link.map(|index| links[index]);
                            target[x + column] = cell;
                        }
                    }
                }
                for mut image in framed.images {
                    image.x += x as u16;
                    image.y += start + top;
                    canvas.images.push(image);
                }
            }
            if grid {
                let mut x = 0;
                for (column, width) in widths.iter().enumerate() {
                    x += width;
                    if column + 1 < columns {
                        for row in &mut canvas.rows[start..start + height] {
                            if x < row.len() {
                                row[x] = Cell {
                                    ch: '│',
                                    extra: None,
                                    style: edge,
                                    link: None,
                                };
                            }
                        }
                        x += 1;
                    }
                }
            }
        }
    }

    /// The narrowest and the preferred width of an element's content, in cells. Widths
    /// do not depend on the window, so they are kept with the document.
    fn measure(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        depth: usize,
    ) -> (usize, usize) {
        if depth > MAX_DEPTH {
            return (1, 1);
        }
        if let Some(known) = self.measures.borrow().get(&element.id()) {
            return *known;
        }
        let measured = self.measure_uncached(element, style, depth);
        self.measures.borrow_mut().insert(element.id(), measured);
        measured
    }

    fn measure_uncached(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        depth: usize,
    ) -> (usize, usize) {
        let extra = style.padding.left
            + style.padding.right
            + usize::from(style.border.left)
            + usize::from(style.border.right);
        if let Some(Length::Cells(cells)) = style.width {
            let cells = cells.round() as usize;
            return (cells, cells);
        }
        if style.display == Display::Table {
            let rows = self.table_rows(element, style);
            let padding = self.cell_padding(element);
            let mut low = 0;
            let mut high = 0;
            for (_, _, cells) in rows {
                let mut row_low = 0;
                let mut row_high = 0;
                for (cell, cell_style, _) in cells {
                    let (cell_low, cell_high) = self.measure(cell, &cell_style, depth + 1);
                    let pad = padding.left + padding.right;
                    row_low += cell_low + pad;
                    row_high += cell_high + pad;
                }
                low = low.max(row_low);
                high = high.max(row_high);
            }
            return (low + extra, high + extra);
        }
        let mut low = 0;
        let mut line = 0;
        let mut high = 0;
        for child in element.children() {
            match child.value() {
                Node::Text(text) => {
                    let text = if style.pre {
                        text.to_string()
                    } else {
                        collapse(text)
                    };
                    let (text, pictures) = self.measure_pictures(&text);
                    low = low.max(pictures);
                    line += pictures;
                    // Long words are cut to fit, so they never force the page wider.
                    for word in text.split_whitespace() {
                        low = low.max(word.width().min(30));
                    }
                    line += text.width();
                }
                Node::Element(_) => {
                    let Some(child) = ElementRef::wrap(child) else {
                        continue;
                    };
                    let child_style = self.compute(child, style);
                    if child_style.display == Display::None {
                        continue;
                    }
                    if child.value().name() == "br" {
                        high = high.max(line);
                        line = 0;
                        continue;
                    }
                    let (child_low, child_high) = self.measure(child, &child_style, depth + 1);
                    low = low.max(child_low);
                    if child_style.is_block_level() {
                        high = high.max(line).max(child_high);
                        line = 0;
                    } else {
                        line += child_high;
                    }
                }
                _ => {}
            }
        }
        high = high.max(line);
        (low.max(1) + extra, high.max(1) + extra)
    }

    /// Takes picture markers out of text, returning the rest and the widest picture.
    fn measure_pictures(&mut self, text: &str) -> (String, usize) {
        let mut rest = text;
        let mut output = String::new();
        let mut widest = 0;
        while let Some(start) = rest.find(IMAGE_START) {
            output.push_str(&rest[..start]);
            let Some(end) = rest[start..].find(IMAGE_END) else {
                break;
            };
            if let Ok(slot) = rest[start + IMAGE_START.len_utf8()..start + end].parse::<usize>() {
                let width = match (self.image_size)(slot, 200) {
                    Ok((columns, _)) => usize::from(columns),
                    Err(Some(label)) => label.width(),
                    Err(None) => 0,
                };
                widest = widest.max(width);
            }
            rest = &rest[start + end + IMAGE_END.len_utf8()..];
        }
        output.push_str(rest);
        (output, widest)
    }

    // ---- inline layout ----

    fn text_style(&self, style: &Computed) -> Style {
        let mut result = Style::default();
        if let Some(color) = style.color {
            result = result.fg(rgb(color));
        } else if !self.options.original {
            result = result.fg(rgb(self.options.text));
        }
        if let Some(background) = style.background {
            result = result.bg(rgb(background));
        }
        let mut modifiers = Modifier::empty();
        if style.bold {
            modifiers |= Modifier::BOLD;
        }
        if style.italic {
            modifiers |= Modifier::ITALIC;
        }
        if style.underline {
            modifiers |= Modifier::UNDERLINED;
        }
        if style.strike {
            modifiers |= Modifier::CROSSED_OUT;
        }
        result.add_modifier(modifiers)
    }

    fn text_pieces(&mut self, text: &str, style: &Computed, pieces: &mut Vec<Piece>) {
        let text_style = self.text_style(style);
        let push_word = |word: &str, space: bool, pieces: &mut Vec<Piece>| {
            let word = if style.uppercase {
                word.to_uppercase()
            } else {
                word.to_owned()
            };
            // Picture markers from the message's <img> tags become boxes in the line.
            let mut rest = word.as_str();
            while let Some(start) = rest.find(IMAGE_START) {
                if start > 0 {
                    pieces.push(Piece::Word {
                        text: rest[..start].to_owned(),
                        style: text_style,
                        link: style.link.clone(),
                        space: false,
                    });
                }
                let Some(end) = rest[start..].find(IMAGE_END) else {
                    break;
                };
                if let Ok(slot) = rest[start + IMAGE_START.len_utf8()..start + end].parse::<usize>()
                {
                    pieces.push(Piece::Atom {
                        width: 0,
                        height: 0,
                        content: Atom::Image(slot),
                        link: style.link.clone(),
                    });
                }
                rest = &rest[start + end + IMAGE_END.len_utf8()..];
            }
            if !rest.is_empty() || space {
                pieces.push(Piece::Word {
                    text: rest.to_owned(),
                    style: text_style,
                    link: style.link.clone(),
                    space,
                });
            }
        };
        if style.pre {
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    pieces.push(Piece::Break);
                }
                let line = line.replace('\t', "    ");
                if !line.is_empty() {
                    push_word(&line, false, pieces);
                }
            }
            return;
        }
        let collapsed = collapse(text);
        if collapsed.is_empty() {
            return;
        }
        if collapsed == " " {
            if let Some(Piece::Word { space, .. }) = pieces.last_mut() {
                *space = true;
            }
            return;
        }
        let leading = collapsed.starts_with(' ');
        if leading && let Some(Piece::Word { space, .. }) = pieces.last_mut() {
            *space = true;
        }
        let words: Vec<&str> = collapsed
            .split(' ')
            .filter(|word| !word.is_empty())
            .collect();
        let trailing = collapsed.ends_with(' ');
        for (index, word) in words.iter().enumerate() {
            let space = index + 1 < words.len() || trailing;
            push_word(word, space, pieces);
        }
    }

    fn inline(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        width: usize,
        pieces: &mut Vec<Piece>,
        depth: usize,
    ) {
        let tag = element.value().name();
        if tag == "br" {
            pieces.push(Piece::Break);
            return;
        }
        // Buttons: links and spans with their own background and padding become boxes.
        let button = style.display == Display::InlineBlock
            || (style.background.is_some()
                && (style.padding.left + style.padding.right + style.padding.top > 0
                    || style.border.any()));
        if button && depth < MAX_DEPTH {
            let (_, preferred) = self.measure(element, style, depth);
            let mut box_width = style
                .width
                .map_or(preferred, |length| length.resolve(width));
            box_width = box_width.clamp(1, width);
            let fill = match style.background {
                Some(background) => self.fill(Some(background)),
                None => Style::default(),
            };
            let inner = box_width
                .saturating_sub(
                    style.padding.left
                        + style.padding.right
                        + usize::from(style.border.left)
                        + usize::from(style.border.right),
                )
                .max(1);
            let mut content = Canvas::new(inner, fill);
            let mut content_style = style.clone();
            content_style.align = Align::Center;
            let mut nested = Vec::new();
            self.inline_children(element, &content_style, inner, &mut nested, depth);
            self.flush_inline(&mut nested, &content_style, inner, &mut content);
            content.trim_trailing_blank();
            let framed = self.frame(content, style, box_width, fill);
            let height = framed.rows.len().max(1);
            pieces.push(Piece::Atom {
                width: box_width,
                height,
                content: Atom::Box(framed),
                link: style.link.clone(),
            });
            return;
        }
        self.inline_children(element, style, width, pieces, depth);
    }

    fn inline_children(
        &mut self,
        element: ElementRef<'_>,
        style: &Computed,
        width: usize,
        pieces: &mut Vec<Piece>,
        depth: usize,
    ) {
        for child in element.children() {
            match child.value() {
                Node::Text(text) => self.text_pieces(text, style, pieces),
                Node::Element(_) => {
                    let Some(child) = ElementRef::wrap(child) else {
                        continue;
                    };
                    let child_style = self.compute(child, style);
                    if child_style.display == Display::None || child_style.hidden {
                        continue;
                    }
                    if depth >= MAX_DEPTH {
                        continue;
                    }
                    if child_style.is_block_level() {
                        // A block inside an inline box breaks the line around it.
                        pieces.push(Piece::Break);
                        self.inline_children(child, &child_style, width, pieces, depth + 1);
                        pieces.push(Piece::Break);
                    } else {
                        self.inline(child, &child_style, width, pieces, depth + 1);
                    }
                }
                _ => {}
            }
        }
    }

    /// Breaks inline pieces into lines and draws them.
    fn flush_inline(
        &mut self,
        pieces: &mut Vec<Piece>,
        style: &Computed,
        width: usize,
        canvas: &mut Canvas,
    ) {
        if pieces.is_empty() {
            return;
        }
        let pieces = std::mem::take(pieces);
        if pieces.iter().all(|piece| match piece {
            Piece::Word { text, .. } => text.trim().is_empty(),
            Piece::Break => true,
            Piece::Atom { .. } => false,
        }) {
            return;
        }
        canvas.margin(0);
        let fill = canvas.fill;
        let mut lines: Vec<Vec<(usize, Piece)>> = vec![Vec::new()];
        let mut used = 0;
        let mut pending_space = false;
        for piece in pieces {
            let piece = match piece {
                Piece::Atom {
                    content: Atom::Image(slot),
                    link,
                    ..
                } => {
                    let room = u16::try_from(width).unwrap_or(u16::MAX);
                    match (self.image_size)(slot, room) {
                        Ok((columns, rows)) => Piece::Atom {
                            width: usize::from(columns).min(width),
                            height: usize::from(rows).max(1),
                            content: Atom::Image(slot),
                            link,
                        },
                        Err(Some(label)) => Piece::Word {
                            text: label,
                            style: self.text_style(style).add_modifier(Modifier::DIM),
                            link,
                            space: true,
                        },
                        Err(None) => continue,
                    }
                }
                other => other,
            };
            let piece_width = match &piece {
                Piece::Word { text, .. } => text.width(),
                Piece::Atom { width, .. } => *width,
                Piece::Break => {
                    lines.push(Vec::new());
                    used = 0;
                    pending_space = false;
                    continue;
                }
            };
            let space = usize::from(pending_space && used > 0);
            if used > 0 && used + space + piece_width > width {
                lines.push(Vec::new());
                used = 0;
            } else if space > 0 {
                used += 1;
            }
            // A word wider than the line is cut by columns.
            if let Piece::Word {
                text,
                style: text_style,
                link,
                space: trailing,
            } = &piece
                && piece_width > width
            {
                let mut chunk = String::new();
                let mut chunk_width = 0;
                for character in text.chars() {
                    let character_width = character.width().unwrap_or(0);
                    if chunk_width + character_width > width.saturating_sub(used)
                        && !chunk.is_empty()
                    {
                        lines.last_mut().unwrap().push((
                            used,
                            Piece::Word {
                                text: std::mem::take(&mut chunk),
                                style: *text_style,
                                link: link.clone(),
                                space: false,
                            },
                        ));
                        lines.push(Vec::new());
                        used = 0;
                        chunk_width = 0;
                    }
                    chunk.push(character);
                    chunk_width += character_width;
                }
                lines.last_mut().unwrap().push((
                    used,
                    Piece::Word {
                        text: chunk,
                        style: *text_style,
                        link: link.clone(),
                        space: *trailing,
                    },
                ));
                used += chunk_width;
                pending_space = *trailing;
                continue;
            }
            pending_space = matches!(&piece, Piece::Word { space: true, .. });
            lines.last_mut().unwrap().push((used, piece));
            used += piece_width;
        }
        for line in lines {
            let content_width = line
                .iter()
                .map(|(x, piece)| {
                    x + match piece {
                        Piece::Word { text, .. } => text.width(),
                        Piece::Atom { width, .. } => *width,
                        Piece::Break => 0,
                    }
                })
                .max()
                .unwrap_or(0);
            let shift = match style.align {
                Align::Left => 0,
                Align::Center => width.saturating_sub(content_width) / 2,
                Align::Right => width.saturating_sub(content_width),
            };
            let height = line
                .iter()
                .map(|(_, piece)| match piece {
                    Piece::Atom { height, .. } => *height,
                    _ => 1,
                })
                .max()
                .unwrap_or(1);
            let top = canvas.rows.len();
            for _ in 0..height {
                canvas.push_blank(fill);
            }
            let text_row = top + height - 1;
            let mut previous_end: Option<(usize, Style, Option<usize>)> = None;
            for (x, piece) in line {
                let x = x + shift;
                match piece {
                    Piece::Word {
                        text,
                        style: text_style,
                        link,
                        ..
                    } => {
                        let link = link.map(|url| canvas.link(&url));
                        let cell_style = fill.patch(text_style);
                        // The space between two words of one link belongs to the link.
                        if let Some((end, previous_style, previous_link)) = previous_end
                            && end < x
                            && x - end == 1
                            && previous_link.is_some()
                            && previous_link == link
                            && end < canvas.rows[text_row].len()
                        {
                            canvas.rows[text_row][end] = Cell {
                                ch: ' ',
                                extra: None,
                                style: previous_style,
                                link,
                            };
                        } else if let Some((end, previous_style, _)) = previous_end
                            && x > end
                            && previous_style.bg.is_some()
                            && previous_style.bg == cell_style.bg
                            && x - end == 1
                            && end < canvas.rows[text_row].len()
                        {
                            canvas.rows[text_row][end].style =
                                fill.patch(Style::default().bg(previous_style.bg.unwrap()));
                        }
                        let mut column = x;
                        for character in text.chars() {
                            let character_width = character.width().unwrap_or(0);
                            if character_width == 0 {
                                if column > 0 && column - 1 < canvas.rows[text_row].len() {
                                    let cell = &mut canvas.rows[text_row][column - 1];
                                    let mut extra =
                                        cell.extra.take().map(String::from).unwrap_or_default();
                                    extra.push(character);
                                    cell.extra = Some(extra.into());
                                }
                                continue;
                            }
                            if column + character_width > width {
                                break;
                            }
                            canvas.rows[text_row][column] = Cell {
                                ch: character,
                                extra: None,
                                style: cell_style,
                                link,
                            };
                            for extra in 1..character_width {
                                canvas.rows[text_row][column + extra] = Cell {
                                    ch: '\0',
                                    extra: None,
                                    style: cell_style,
                                    link,
                                };
                            }
                            column += character_width;
                        }
                        previous_end = Some((column, cell_style, link));
                    }
                    Piece::Atom {
                        width: atom_width,
                        height: atom_height,
                        content,
                        link,
                    } => {
                        let link = link.map(|url| canvas.link(&url));
                        let y = top + height - atom_height;
                        match content {
                            Atom::Image(slot) => {
                                canvas.images.push(Placed {
                                    slot,
                                    x: x as u16,
                                    y,
                                    width: atom_width as u16,
                                    height: atom_height as u16,
                                });
                                if let Some(link) = link {
                                    for row in &mut canvas.rows[y..y + atom_height] {
                                        for cell in row.iter_mut().skip(x).take(atom_width) {
                                            cell.link = Some(link);
                                        }
                                    }
                                }
                            }
                            Atom::Box(framed) => {
                                let links: Vec<usize> =
                                    framed.links.iter().map(|url| canvas.link(url)).collect();
                                for (offset, row) in framed.rows.into_iter().enumerate() {
                                    for (column, mut cell) in row.into_iter().enumerate() {
                                        if x + column < width {
                                            cell.link =
                                                cell.link.map(|index| links[index]).or(link);
                                            canvas.rows[y + offset][x + column] = cell;
                                        }
                                    }
                                }
                                for mut image in framed.images {
                                    image.x += x as u16;
                                    image.y += y;
                                    canvas.images.push(image);
                                }
                            }
                        }
                        previous_end = None;
                    }
                    Piece::Break => {}
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// CSS

fn collect_rules(document: &Html) -> HashMap<NodeId, Vec<Rule>> {
    let mut rules: HashMap<NodeId, Vec<Rule>> = HashMap::new();
    let Ok(style_selector) = Selector::parse("style") else {
        return rules;
    };
    let mut order = 0;
    for style in document.select(&style_selector) {
        let css: String = style.text().collect();
        for (selectors, body) in style_blocks(&css) {
            let declarations = declarations(&body);
            if declarations.is_empty() {
                continue;
            }
            for selector in selectors.split(',') {
                let selector = selector.trim();
                if selector.is_empty()
                    || selector.contains(':') && !selector.contains(":first-child")
                {
                    continue;
                }
                let Ok(parsed) = Selector::parse(selector) else {
                    continue;
                };
                order += 1;
                let specificity = specificity(selector);
                for element in document.select(&parsed) {
                    rules.entry(element.id()).or_default().push(Rule {
                        order,
                        specificity,
                        declarations: declarations.clone(),
                    });
                }
            }
        }
    }
    for list in rules.values_mut() {
        list.sort_by_key(|rule| (rule.specificity, rule.order));
    }
    rules
}

/// Top-level `selector { declarations }` blocks. At-rules such as `@media` are
/// skipped with their contents: a terminal is neither a phone nor a printer.
fn style_blocks(css: &str) -> Vec<(String, String)> {
    let css = strip_comments(css);
    let mut blocks = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let selector = rest[..open].trim().to_owned();
        let mut depth = 0;
        let mut close = None;
        for (index, character) in rest[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            break;
        };
        let selector = selector
            .rsplit(['}', ';'])
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if !selector.starts_with('@') && !selector.is_empty() {
            blocks.push((selector, rest[open + 1..close].to_owned()));
        }
        rest = &rest[close + 1..];
    }
    blocks
}

fn strip_comments(css: &str) -> String {
    let mut output = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        output.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return output,
        }
    }
    output.push_str(rest);
    output.replace("<!--", "").replace("-->", "")
}

fn specificity(selector: &str) -> u32 {
    let ids = selector.matches('#').count() as u32;
    let classes = (selector.matches('.').count() + selector.matches('[').count()) as u32;
    let elements = selector
        .split(|c: char| c.is_whitespace() || ">+~".contains(c))
        .filter(|part| part.chars().next().is_some_and(|c| c.is_ascii_alphabetic()))
        .count() as u32;
    ids * 10_000 + classes * 100 + elements
}

fn declarations(body: &str) -> Vec<(String, String)> {
    body.split(';')
        .filter_map(|declaration| {
            let (name, value) = declaration.split_once(':')?;
            let name = name.trim().to_ascii_lowercase();
            let value = value
                .trim()
                .trim_end_matches("!important")
                .trim()
                .to_owned();
            (!name.is_empty() && !value.is_empty()).then_some((name, value))
        })
        .collect()
}

fn apply(style: &mut Computed, name: &str, value: &str) {
    let lower = value.to_ascii_lowercase();
    match name {
        "display" => {
            style.display = match lower.as_str() {
                "none" => Display::None,
                "block" | "flex" | "grid" => Display::Block,
                "inline-block" | "inline-flex" => Display::InlineBlock,
                "inline" => Display::Inline,
                "table" => Display::Table,
                "table-row" => Display::Row,
                "table-cell" => Display::Cell,
                "list-item" => Display::ListItem,
                _ => style.display,
            };
        }
        "visibility" if lower == "hidden" => style.hidden = true,
        "opacity" if lower.trim() == "0" => style.hidden = true,
        "mso-hide" if lower == "all" => style.hidden = true,
        "max-height" | "height" if is_zero(&lower) => style.hidden = true,
        "font-size" if is_zero(&lower) || lower == "1px" => style.hidden = true,
        "color" => {
            if let Some(color) = parse_color(&lower) {
                style.color = Some(color);
            }
        }
        "background-color" => {
            if let Some(color) = parse_color(&lower) {
                style.background = Some(color);
            } else if lower == "transparent" {
                style.background = None;
            }
        }
        "background" => {
            if let Some(color) = lower.split_whitespace().find_map(parse_color) {
                style.background = Some(color);
            }
        }
        "font-weight" => {
            style.bold = lower == "bold"
                || lower == "bolder"
                || lower.parse::<u32>().is_ok_and(|weight| weight >= 600);
        }
        "font-style" => style.italic = lower == "italic" || lower == "oblique",
        "text-decoration" | "text-decoration-line" => {
            style.underline = lower.contains("underline");
            style.strike = lower.contains("line-through");
        }
        "text-transform" => style.uppercase = lower == "uppercase",
        "text-align" => {
            style.align = match lower.as_str() {
                "center" => Align::Center,
                "right" | "end" => Align::Right,
                "left" | "start" | "justify" => Align::Left,
                _ => style.align,
            };
        }
        "white-space" => style.pre = lower.starts_with("pre"),
        "width" => {
            if let Some(length) = parse_length(&lower, PX_COLUMN) {
                style.width = Some(length);
            }
        }
        "max-width" => {
            if let Some(length) = parse_length(&lower, PX_COLUMN) {
                style.max_width = Some(length);
            }
        }
        "float" => style.float_right = lower == "right",
        "vertical-align" => {
            style.valign_top = matches!(lower.as_str(), "top" | "baseline" | "text-top")
        }
        "padding" => style.padding = sides(&lower),
        "padding-top" => style.padding.top = rows(&lower),
        "padding-bottom" => style.padding.bottom = rows(&lower),
        "padding-left" => style.padding.left = columns(&lower),
        "padding-right" => style.padding.right = columns(&lower),
        "margin" => {
            let parts: Vec<&str> = lower.split_whitespace().collect();
            style.center |=
                parts.get(1).copied() == Some("auto") || (parts.len() == 1 && parts[0] == "auto");
            let parsed = sides(&lower);
            style.margin.top = parsed.top;
            style.margin.bottom = parsed.bottom;
        }
        "margin-top" => style.margin.top = rows(&lower),
        "margin-bottom" => style.margin.bottom = rows(&lower),
        "margin-left" | "margin-right" if lower == "auto" => style.center = true,
        "border" => border(style, &lower, [true, true, true, true]),
        "border-top" => border(style, &lower, [true, false, false, false]),
        "border-right" => border(style, &lower, [false, true, false, false]),
        "border-bottom" => border(style, &lower, [false, false, true, false]),
        "border-left" => border(style, &lower, [false, false, false, true]),
        "border-color" => {
            if let Some(color) = parse_color(&lower) {
                style.border.color = Some(color);
            }
        }
        _ => {}
    }
}

fn is_zero(value: &str) -> bool {
    let number = value.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    number.parse::<f32>().is_ok_and(|number| number == 0.0)
}

fn border(style: &mut Computed, value: &str, which: [bool; 4]) {
    let none = value.contains("none")
        || value.contains("hidden")
        || value.split_whitespace().any(is_zero)
        || value == "0";
    if let Some(color) = value.split_whitespace().find_map(parse_color) {
        style.border.color = Some(color);
    }
    let set = !none;
    let [top, right, bottom, left] = which;
    if top {
        style.border.top = set;
    }
    if right {
        style.border.right = set;
    }
    if bottom {
        style.border.bottom = set;
    }
    if left {
        style.border.left = set;
    }
}

fn pixels(value: &str) -> Option<f32> {
    let value = value.trim();
    if let Some(number) = value.strip_suffix("px") {
        return number.trim().parse().ok();
    }
    if let Some(number) = value
        .strip_suffix("em")
        .or_else(|| value.strip_suffix("rem"))
    {
        return number.trim().parse::<f32>().ok().map(|em| em * 16.0);
    }
    if let Some(number) = value.strip_suffix("pt") {
        return number.trim().parse::<f32>().ok().map(|pt| pt * 4.0 / 3.0);
    }
    value.parse().ok()
}

fn rows(value: &str) -> usize {
    pixels(value)
        .map_or(0, |px| (px / PX_ROW).round().max(0.0) as usize)
        .min(6)
}

fn columns(value: &str) -> usize {
    pixels(value)
        .map_or(0, |px| (px / PX_COLUMN).round().max(0.0) as usize)
        .min(12)
}

fn sides(value: &str) -> Sides {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let (top, right, bottom, left) = match parts.as_slice() {
        [all] => (*all, *all, *all, *all),
        [vertical, horizontal] => (*vertical, *horizontal, *vertical, *horizontal),
        [top, horizontal, bottom] => (*top, *horizontal, *bottom, *horizontal),
        [top, right, bottom, left, ..] => (*top, *right, *bottom, *left),
        [] => return Sides::default(),
    };
    Sides {
        top: rows(top),
        right: columns(right),
        bottom: rows(bottom),
        left: columns(left),
    }
}

fn parse_length(value: &str, per_cell: f32) -> Option<Length> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent.trim().parse().ok().map(Length::Percent);
    }
    if value == "auto" {
        return None;
    }
    pixels(value).map(|px| Length::Cells(px / per_cell))
}

fn parse_attribute_length(value: &str) -> Option<Length> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent.trim().parse().ok().map(Length::Percent);
    }
    value
        .trim_end_matches("px")
        .parse::<f32>()
        .ok()
        .filter(|px| *px > 0.0)
        .map(|px| Length::Cells(px / PX_COLUMN))
}

fn collapse(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut space = false;
    for character in text.chars() {
        if character.is_whitespace() && character != '\u{a0}' {
            space = true;
        } else {
            if space {
                output.push(' ');
                space = false;
            }
            // Zero-width spacers that mail uses to pad preview text.
            if !matches!(
                character,
                '\u{200b}' | '\u{200c}' | '\u{034f}' | '\u{feff}' | '\u{ad}'
            ) {
                output.push(if character == '\u{a0}' {
                    ' '
                } else {
                    character
                });
            }
        }
    }
    if space {
        output.push(' ');
    }
    output
}

pub fn rgb((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

pub fn parse_color(value: &str) -> Option<Rgb> {
    let value = value.trim().trim_end_matches(';').to_ascii_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .filter_map(|c| c.to_digit(16).map(|d| d as u8))
            .collect();
        if digits.len() != hex.len() {
            return None;
        }
        return match digits.as_slice() {
            [r, g, b] | [r, g, b, _] => Some((r * 17, g * 17, b * 17)),
            [r1, r2, g1, g2, b1, b2] | [r1, r2, g1, g2, b1, b2, _, _] => {
                Some((r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2))
            }
            _ => None,
        };
    }
    if let Some(inner) = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner
            .split([',', ' ', '/'])
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() >= 4
            && parts[3]
                .trim_end_matches('%')
                .parse::<f32>()
                .is_ok_and(|alpha| alpha == 0.0)
        {
            return None;
        }
        let channel = |part: &str| -> Option<u8> {
            match part.strip_suffix('%') {
                Some(percent) => percent
                    .parse::<f32>()
                    .ok()
                    .map(|p| (p * 2.55).round() as u8),
                None => part.parse::<f32>().ok().map(|v| v.clamp(0.0, 255.0) as u8),
            }
        };
        return Some((
            channel(parts.first()?)?,
            channel(parts.get(1)?)?,
            channel(parts.get(2)?)?,
        ));
    }
    NAMED
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, color)| *color)
}

const NAMED: &[(&str, Rgb)] = &[
    ("black", (0, 0, 0)),
    ("white", (255, 255, 255)),
    ("red", (255, 0, 0)),
    ("green", (0, 128, 0)),
    ("blue", (0, 0, 255)),
    ("yellow", (255, 255, 0)),
    ("orange", (255, 165, 0)),
    ("purple", (128, 0, 128)),
    ("gray", (128, 128, 128)),
    ("grey", (128, 128, 128)),
    ("silver", (192, 192, 192)),
    ("maroon", (128, 0, 0)),
    ("navy", (0, 0, 128)),
    ("teal", (0, 128, 128)),
    ("olive", (128, 128, 0)),
    ("lime", (0, 255, 0)),
    ("aqua", (0, 255, 255)),
    ("cyan", (0, 255, 255)),
    ("fuchsia", (255, 0, 255)),
    ("magenta", (255, 0, 255)),
    ("pink", (255, 192, 203)),
    ("brown", (165, 42, 42)),
    ("gold", (255, 215, 0)),
    ("indigo", (75, 0, 130)),
    ("violet", (238, 130, 238)),
    ("crimson", (220, 20, 60)),
    ("coral", (255, 127, 80)),
    ("salmon", (250, 128, 114)),
    ("tomato", (255, 99, 71)),
    ("orangered", (255, 69, 0)),
    ("darkred", (139, 0, 0)),
    ("darkgreen", (0, 100, 0)),
    ("darkblue", (0, 0, 139)),
    ("darkgray", (169, 169, 169)),
    ("darkgrey", (169, 169, 169)),
    ("dimgray", (105, 105, 105)),
    ("dimgrey", (105, 105, 105)),
    ("lightgray", (211, 211, 211)),
    ("lightgrey", (211, 211, 211)),
    ("gainsboro", (220, 220, 220)),
    ("whitesmoke", (245, 245, 245)),
    ("ghostwhite", (248, 248, 255)),
    ("aliceblue", (240, 248, 255)),
    ("lightblue", (173, 216, 230)),
    ("skyblue", (135, 206, 235)),
    ("steelblue", (70, 130, 180)),
    ("royalblue", (65, 105, 225)),
    ("dodgerblue", (30, 144, 255)),
    ("deepskyblue", (0, 191, 255)),
    ("cornflowerblue", (100, 149, 237)),
    ("midnightblue", (25, 25, 112)),
    ("slategray", (112, 128, 144)),
    ("slategrey", (112, 128, 144)),
    ("lightslategray", (119, 136, 153)),
    ("seagreen", (46, 139, 87)),
    ("forestgreen", (34, 139, 34)),
    ("limegreen", (50, 205, 50)),
    ("lightgreen", (144, 238, 144)),
    ("mediumseagreen", (60, 179, 113)),
    ("beige", (245, 245, 220)),
    ("ivory", (255, 255, 240)),
    ("linen", (250, 240, 230)),
    ("lavender", (230, 230, 250)),
    ("mintcream", (245, 255, 250)),
    ("honeydew", (240, 255, 240)),
    ("azure", (240, 255, 255)),
    ("snow", (255, 250, 250)),
    ("seashell", (255, 245, 238)),
    ("oldlace", (253, 245, 230)),
    ("floralwhite", (255, 250, 240)),
    ("lightyellow", (255, 255, 224)),
    ("lemonchiffon", (255, 250, 205)),
    ("khaki", (240, 230, 140)),
    ("tan", (210, 180, 140)),
    ("chocolate", (210, 105, 30)),
    ("firebrick", (178, 34, 34)),
    ("darkorange", (255, 140, 0)),
    ("hotpink", (255, 105, 180)),
    ("deeppink", (255, 20, 147)),
    ("plum", (221, 160, 221)),
    ("orchid", (218, 112, 214)),
    ("turquoise", (64, 224, 208)),
    ("darkslategray", (47, 79, 79)),
    ("darkslategrey", (47, 79, 79)),
];

/// Relative luminance per WCAG.
pub fn luminance((r, g, b): Rgb) -> f64 {
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

pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Coloured enough to keep in the themed mode: not a white, black or grey page.
fn colourful((r, g, b): Rgb) -> bool {
    r.max(g).max(b) - r.min(g).min(b) >= 32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str, width: u16, original: bool) -> Page {
        let options = Options {
            original,
            surface: (48, 48, 48),
            text: (208, 208, 208),
            link: (95, 215, 255),
        };
        let mut size = |slot: usize, room: u16| -> Result<(u16, u16), Option<String>> {
            if slot == 0 {
                Ok((room.min(20), 4))
            } else {
                Err(Some("[picture]".into()))
            }
        };
        render_document(&Document::parse(html), width, options, &mut size)
    }

    fn text(page: &Page) -> Vec<String> {
        page.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    const CARD: &str = r##"<html><head><style>
        .card { background:#ffffff; border:1px solid #d8dde4 }
        .btn { background:#2f6fed; color:#fff; padding:12px 24px }
        @media (max-width:600px) { .card { width:100% } }
        </style></head><body bgcolor="#eef1f5">
        <div style="display:none;max-height:0">secret preview text</div>
        <table width="100%"><tr><td align="center">
          <table class="card" width="600"><tr><td style="padding:16px">
            <p>Hello Ada</p>
            <table border="1"><tr><th>Name</th><th>Value</th></tr><tr><td>a</td><td>b</td></tr></table>
            <p style="text-align:center"><a class="btn" href="https://x.test/go">Open</a></p>
          </td></tr></table>
        </td></tr></table></body></html>"##;

    #[test]
    fn draws_mail_layouts_like_a_browser() {
        let page = page(CARD, 100, true);
        let lines = text(&page);
        let all = lines.join("\n");
        assert!(
            !all.contains("secret preview"),
            "hidden preheaders stay hidden"
        );
        // The 600px card is 75 columns, centred, with its border.
        let top = lines.iter().find(|line| line.contains('┌')).unwrap();
        assert_eq!(top.find('┌').unwrap(), (100 - 75) / 2);
        // Text inside the centred cell starts at the left again, as in mail clients.
        let hello = lines.iter().find(|line| line.contains("Hello")).unwrap();
        assert!(hello.find("Hello").unwrap() < 20, "{hello:?}");
        // A table with border="1" gets lines between cells.
        assert!(all.contains('┼'));
        // The button is a box with its own background, and clickable.
        let button = page
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("Open"))
            .unwrap();
        assert_eq!(button.style.bg, Some(Color::Rgb(47, 111, 237)));
        assert!(
            page.links
                .iter()
                .any(|link| link.url == "https://x.test/go")
        );
        // The page background fills every row.
        assert!(
            page.lines
                .iter()
                .all(|line| line.spans.iter().any(|span| span.style.bg.is_some()))
        );
    }

    #[test]
    fn theme_colours_drop_page_backgrounds_and_keep_buttons() {
        let page = page(CARD, 100, false);
        let backgrounds: Vec<Color> = page
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.style.bg)
            .collect();
        assert!(!backgrounds.contains(&Color::Rgb(255, 255, 255)));
        assert!(backgrounds.contains(&Color::Rgb(47, 111, 237)));
    }

    #[test]
    fn fixed_width_pages_keep_their_width() {
        let page = page(CARD, 40, true);
        assert!(page.width >= 75, "{}", page.width);
        // A long word never forces the page wider than the window.
        let page = super::tests::page(&format!("<p>{}</p>", "x".repeat(400)), 40, true);
        assert_eq!(page.width, 40);
    }

    #[test]
    fn colours_parse_like_css() {
        assert_eq!(parse_color("#abc"), Some((170, 187, 204)));
        assert_eq!(parse_color("rgb(10, 20, 30)"), Some((10, 20, 30)));
        assert_eq!(parse_color("rgba(0,0,0,0)"), None);
        assert_eq!(parse_color("LightBlue"), Some((173, 216, 230)));
        assert_eq!(parse_color("#12345"), None);
    }

    /// Random mixes of tags, attributes and text must never panic, and every row must
    /// be exactly as wide as the page. A seeded generator keeps failures repeatable.
    #[test]
    fn survives_random_html() {
        const PARTS: &[&str] = &[
            "<table>",
            "</table>",
            "<tr>",
            "</tr>",
            "<td colspan=3>",
            "<td width=900>",
            "</td>",
            "<div style='padding:40px;border:2px solid red;width:50%'>",
            "</div>",
            "<p>",
            "</p>",
            "<ul><li>",
            "</li></ul>",
            "<ol><li>",
            "<a href='https://x.test'>",
            "</a>",
            "<br>",
            "<pre>\tcode  here\n</pre>",
            "<center>",
            "</center>",
            "<b>",
            "</b>",
            "<hr>",
            "<span style='background:#f00;padding:9px'>",
            "</span>",
            "<table border=1 cellpadding=30>",
            "word ",
            "averyveryveryveryverylongwordthatneverbreaks ",
            "漢字テキスト ",
            "🎉 ",
            "\u{E000}0\u{E001}",
            "\u{E000}7\u{E001}",
            "<td valign=top>",
            "<blockquote>",
            "</blockquote>",
            "&nbsp;",
            "<style>p{x:y}}</style>",
            "<th align=right>",
            "<font color=#123>",
            "</font>",
            "<img src=x>",
        ];
        let mut seed: u64 = 0x5eed;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300 {
            let length = 5 + (next() % 60) as usize;
            let html: String = (0..length)
                .map(|_| PARTS[(next() % PARTS.len() as u64) as usize])
                .collect();
            let width = 8 + (next() % 130) as u16;
            for original in [true, false] {
                let page = page(&html, width, original);
                for line in &page.lines {
                    let columns: usize = line.spans.iter().map(|span| span.content.width()).sum();
                    assert_eq!(
                        columns,
                        usize::from(page.width),
                        "width {width} html {html:?}"
                    );
                }
                for link in &page.links {
                    assert!(link.start < link.end && link.end <= page.width);
                }
                for image in &page.images {
                    assert!(image.x + image.width <= page.width);
                }
            }
        }
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let html = "<div><table><tr><td>".repeat(400) + "deep";
        let page = page(&html, 60, true);
        assert!(!page.lines.is_empty());
    }
}

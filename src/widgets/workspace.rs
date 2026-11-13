use std::collections::HashSet;

use crate::{
    mail::{Mail, parents, roots},
    theme,
    widgets::mail_card::MailCard,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use rataflow::{
    Background, BackgroundVariant, Edge, EdgeStyle, FitViewOptions, Flow, FlowEvent,
    HandlePosition, MiniMap, MiniMapPosition, Node, SelectionReveal, StepEdge, Sugiyama,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders},
};

const CARD_WIDTH: usize = 40;
const CARD_HEIGHT: usize = 9;
const HORIZONTAL_LAYOUT_WIDTH: u16 = 104;
/// Columns the viewport moves for each shift+arrow press.
const PAN_STEP: f64 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowLayout {
    Horizontal,
    Vertical,
}

pub struct Workspace {
    flow: Flow<MailCard, StepEdge>,
    mails: Vec<Mail>,
    selected_mail_id: String,
    layout: FlowLayout,
}

/// The viewport delta for a pan key, if this key pans.
fn pan_offset(key: KeyEvent) -> Option<(f64, f64)> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::ALT) => {
            match character.to_ascii_lowercase() {
                'h' => Some((PAN_STEP, 0.0)),
                'l' => Some((-PAN_STEP, 0.0)),
                'k' => Some((0.0, PAN_STEP)),
                'j' => Some((0.0, -PAN_STEP)),
                _ => None,
            }
        }
        KeyCode::Left if shift => Some((PAN_STEP, 0.0)),
        KeyCode::Right if shift => Some((-PAN_STEP, 0.0)),
        KeyCode::Up if shift => Some((0.0, PAN_STEP)),
        KeyCode::Down if shift => Some((0.0, -PAN_STEP)),
        _ => None,
    }
}

impl Workspace {
    pub fn new(mails: &[Mail], selected_mail_id: &str) -> Self {
        let layout = FlowLayout::Horizontal;
        Self {
            flow: conversation_flow(mails, selected_mail_id, layout),
            mails: mails.to_vec(),
            selected_mail_id: selected_mail_id.into(),
            layout,
        }
    }

    pub fn show_conversation(&mut self, mails: &[Mail], selected_mail_id: &str) {
        self.mails = mails.to_vec();
        self.selected_mail_id = selected_mail_id.into();
        self.flow = conversation_flow(&self.mails, &self.selected_mail_id, self.layout);
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let title = crate::i18n::t(if focused {
            " workspace • active "
        } else {
            " workspace "
        });
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::border(focused))
            .title_style(theme::title(focused))
            .title(title);
        let inner = block.inner(area);
        let layout = if inner.width < HORIZONTAL_LAYOUT_WIDTH {
            FlowLayout::Vertical
        } else {
            FlowLayout::Horizontal
        };
        if layout != self.layout {
            self.layout = layout;
            self.flow = conversation_flow(&self.mails, &self.selected_mail_id, layout);
        }

        frame.render_widget(block, area);
        frame.render_widget(
            Background::new(&self.flow)
                .variant(BackgroundVariant::Dots)
                .gap(6, 3),
            inner,
        );
        frame.render_widget(&mut self.flow, inner);
        if self.needs_minimap(inner) {
            let map = MiniMap::new(&self.flow)
                .position(MiniMapPosition::BottomRight)
                .size(20, 7)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::border(false))
                        .title_style(theme::muted())
                        .title(crate::i18n::t(" map ")),
                );
            frame.render_widget(map, inner);
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<String> {
        let clicked =
            self.flow
                .handle_mouse_event(mouse)
                .into_events()
                .find_map(|event| match event {
                    FlowEvent::NodeClicked { node_id } => Some(node_id),
                    _ => None,
                });
        if let Some(mail_id) = &clicked {
            self.selected_mail_id = mail_id.clone();
            self.update_edge_styles();
        }
        clicked
    }

    /// Moves the viewport with shift+arrows or alt+hjkl, so a large
    /// conversation can be read without a mouse, and selects nodes with the
    /// plain keys.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some((dx, dy)) = pan_offset(key) {
            self.flow.pan(dx, dy);
            return;
        }
        match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                self.flow.select_prev_node();
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('l') => {
                self.flow.select_next_node();
            }
            KeyCode::Char('+' | '=') => self.flow.zoom_in(),
            KeyCode::Char('-' | '_') => self.flow.zoom_out(),
            KeyCode::Char('0') => self.flow.reset_zoom(),
            KeyCode::Char('f') => self.flow.request_fit_view(),
            KeyCode::Char('c') => self.flow.center_on_selected(),
            _ => {}
        }
        if let Some(mail_id) = self.flow.first_selected_node_id() {
            self.selected_mail_id = mail_id;
            self.update_edge_styles();
        }
    }

    pub fn selected_mail_id(&self) -> Option<String> {
        self.flow.first_selected_node_id()
    }

    fn needs_minimap(&self, area: Rect) -> bool {
        if area.width < 48 || area.height < 12 {
            return false;
        }

        let bounds = self
            .flow
            .nodes()
            .map(Node::bounds)
            .reduce(|a, b| a.union(&b));
        let Some(bounds) = bounds else {
            return false;
        };
        let zoom = self.flow.viewport.zoom;

        bounds.width() * zoom > area.width.saturating_sub(4) as f64
            || bounds.height() * zoom > area.height.saturating_sub(4) as f64
    }

    fn update_edge_styles(&mut self) {
        let path = selected_path_ids(&self.mails, &self.selected_mail_id);
        for (edge_id, content) in self.flow.edges_content_mut() {
            let target_id = edge_id.strip_prefix("reply-").unwrap_or(edge_id);
            content.style = Some(reply_style(path.contains(target_id)));
        }
    }
}

fn conversation_flow(
    mails: &[Mail],
    selected_mail_id: &str,
    layout: FlowLayout,
) -> Flow<MailCard, StepEdge> {
    let Some(selected) = mails.iter().position(|mail| mail.id == selected_mail_id) else {
        return Flow::new();
    };
    let parents = parents(mails);
    let roots = roots(&parents);
    let conversation: Vec<usize> = (0..mails.len())
        .filter(|index| roots[*index] == roots[selected])
        .collect();
    let selected_path = selected_path_ids(mails, selected_mail_id);
    let conversation: Vec<(&Mail, Option<&str>)> = conversation
        .into_iter()
        .map(|index| {
            (
                &mails[index],
                parents[index].map(|parent| mails[parent].id.as_str()),
            )
        })
        .collect();
    let nodes = conversation
        .iter()
        .enumerate()
        .map(|(index, (mail, _))| {
            Node::new(
                &mail.id,
                (0.0, 0.0),
                (CARD_WIDTH as f64, CARD_HEIGHT as f64),
                MailCard::new(mail, index + 1, conversation.len()),
            )
            .with_source_position(match layout {
                FlowLayout::Horizontal => HandlePosition::Right,
                FlowLayout::Vertical => HandlePosition::Bottom,
            })
            .with_target_position(match layout {
                FlowLayout::Horizontal => HandlePosition::Left,
                FlowLayout::Vertical => HandlePosition::Top,
            })
            .with_selected(mail.id == selected_mail_id)
        })
        .collect();
    let edges: Vec<Edge<StepEdge>> = conversation
        .iter()
        .filter_map(|(mail, parent_id)| {
            let parent_id = (*parent_id)?;
            conversation
                .iter()
                .any(|(parent, _)| parent.id == parent_id)
                .then(|| {
                    let edge = Edge::new(format!("reply-{}", mail.id), parent_id, &mail.id);
                    let edge = match layout {
                        FlowLayout::Horizontal => edge
                            .with_source_side(HandlePosition::Right)
                            .with_target_side(HandlePosition::Left),
                        FlowLayout::Vertical => edge
                            .with_source_side(HandlePosition::Bottom)
                            .with_target_side(HandlePosition::Top),
                    };
                    edge.with_content(
                        StepEdge::default()
                            .with_stem_length(2.0)
                            .with_style(reply_style(selected_path.contains(mail.id.as_str()))),
                    )
                })
        })
        .collect();

    // A malformed thread (such as two messages sharing an ID) shows an empty map rather
    // than taking the client down.
    let Ok(flow) = Flow::with_graph(nodes, edges) else {
        return Flow::new();
    };
    let mut flow = flow
        .with_theme(theme::flow())
        .with_selection_reveal(SelectionReveal::EnsureVisible)
        .with_min_zoom(0.8)
        .with_max_zoom(1.25);
    let sugiyama = match layout {
        FlowLayout::Horizontal => Sugiyama::horizontal(),
        FlowLayout::Vertical => Sugiyama::vertical(),
    }
    .with_node_spacing(3.0)
    .with_rank_spacing(4.0)
    .with_margin(3.0);
    flow.apply_layout(sugiyama);
    flow.request_fit_view_with_options(
        FitViewOptions::default()
            .with_padding(2.0)
            .with_zoom_range(0.8, 1.0),
    );
    flow
}

fn reply_style(active: bool) -> EdgeStyle {
    let style = Style::default().fg(if active {
        theme::accent()
    } else {
        theme::muted_color()
    });
    let style = if active {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    EdgeStyle::default().with_stroke_style(style)
}

fn selected_path_ids<'a>(mails: &'a [Mail], selected_mail_id: &str) -> HashSet<&'a str> {
    let parents = parents(mails);
    let mut ids = HashSet::new();
    let mut current = mails.iter().position(|mail| mail.id == selected_mail_id);

    while let Some(index) = current {
        if !ids.insert(mails[index].id.as_str()) {
            break;
        }
        current = parents[index];
    }

    ids
}

#[cfg(test)]
mod tests {
    use crate::mail::demo_mails;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{FlowLayout, Workspace, conversation_flow, selected_path_ids};

    #[test]
    fn lays_out_conversations_horizontally() {
        let flow = conversation_flow(&demo_mails(), "project-2", FlowLayout::Horizontal);

        assert!(
            flow.node("project-1").unwrap().position.x < flow.node("project-2").unwrap().position.x
        );
        assert!(flow.node("project-2").unwrap().selected);
    }

    #[test]
    fn lays_out_conversations_vertically() {
        let flow = conversation_flow(&demo_mails(), "project-2", FlowLayout::Vertical);

        assert!(
            flow.node("project-1").unwrap().position.y < flow.node("project-2").unwrap().position.y
        );
    }

    #[test]
    fn follows_the_selected_reply_path() {
        let mails = demo_mails();
        let path = selected_path_ids(&mails, "project-3");

        assert_eq!(path.len(), 3);
        assert!(path.contains("project-1"));
        assert!(path.contains("project-3"));
        assert!(!path.contains("weekend-1"));
    }

    #[test]
    fn renders_reference_branches() {
        let mut mails = demo_mails();
        mails.push(
            crate::mail::Mail::new(
                "branch",
                "mert@example.com",
                "re: project update",
                "ship it",
                None,
            )
            .with_references(&["project-1"]),
        );
        let flow = conversation_flow(&mails, "branch", FlowLayout::Horizontal);

        assert!(flow.edge("reply-branch").is_some());
        assert!(flow.node("project-1").is_some());
    }

    #[test]
    fn shifts_the_viewport_with_shift_arrows() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();

        let before = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        let after = workspace.flow.viewport.offset();

        assert!(
            after.x < before.x,
            "shift+right should reveal the right side"
        );
        assert_eq!(workspace.selected_mail_id().as_deref(), Some("project-3"));
    }

    #[test]
    fn pans_with_alt_hjkl() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();

        let before = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT));
        let after_h = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT));
        let after_l = workspace.flow.viewport.offset();
        // Alt+shift+h arrives as an uppercase letter on some terminals.
        workspace.handle_key(KeyEvent::new(
            KeyCode::Char('H'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        let after_shift = workspace.flow.viewport.offset();

        assert!(after_h.x > before.x, "alt+h should reveal the left side");
        assert!(after_l.x < after_h.x, "alt+l should reveal the right side");
        assert!(
            after_shift.x > after_l.x,
            "alt+shift+h should still pan left"
        );
        assert_eq!(workspace.selected_mail_id().as_deref(), Some("project-3"));
    }

    #[test]
    fn leaves_shift_j_and_shift_k_alone() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();

        let before = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT));
        workspace.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT));
        let after = workspace.flow.viewport.offset();

        assert_eq!(after.x, before.x);
        assert_eq!(after.y, before.y);
    }

    #[test]
    fn renders_mail_card_hierarchy() {
        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(screen.contains("workspace"));
        assert!(screen.contains("project update"), "{screen}");
        assert!(screen.contains("ada@example.com"));
        assert!(screen.contains("↵ open"));
    }
}

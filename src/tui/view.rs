use super::{default_provider, protocol_label, text_edit::TextInput, App, Mode};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Table, Wrap},
    Frame,
};

const SELECTED: Style = Style::new().fg(Color::Yellow);

// Reserve a cell for the cursor and measure terminal cells rather than UTF-8
// bytes, so long URLs and wide characters keep the insertion point visible.
fn input_value(input: &TextInput, width: usize, focused: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let cursor = if focused { input.cursor } else { 0 };
    let mut remaining = width - usize::from(focused);
    let mut start = cursor;
    for (offset, ch) in input.value[..cursor].char_indices().rev() {
        let cells = Span::raw(ch.to_string()).width();
        if cells > remaining {
            break;
        }
        remaining -= cells;
        start = offset;
    }
    let mut end = cursor;
    for ch in input.value[cursor..].chars() {
        let cells = Span::raw(ch.to_string()).width();
        if cells > remaining {
            break;
        }
        remaining -= cells;
        end += ch.len_utf8();
    }
    format!(
        "{}{}{}",
        &input.value[start..cursor],
        if focused { "▏" } else { "" },
        &input.value[cursor..end]
    )
}

fn field(label: &str, input: &TextInput, width: u16, focused: bool) -> ListItem<'static> {
    let prefix = format!("{label}: ");
    let available = usize::from(width).saturating_sub(2 + Span::raw(&prefix).width());
    ListItem::new(format!(
        "{prefix}{}",
        input_value(input, available, focused)
    ))
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let areas = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(7),
    ])
    .split(frame.area());
    match &mut app.mode {
        Mode::Provider(edit) => {
            let mut rows: Vec<ListItem> = ["ID", "Name", "Base URL"]
                .iter()
                .enumerate()
                .map(|(i, label)| field(label, &edit.fields[i], areas[0].width, edit.focus == i))
                .collect();
            rows.push(ListItem::new(if edit.fields[3].value.is_empty() {
                "API key: (missing)"
            } else {
                "API key: ******** (configured)"
            }));
            let protocol = match edit.protocol {
                0 => "responses",
                1 => "chat_completions",
                _ => "messages",
            };
            rows.push(ListItem::new(format!("Protocol: ← {protocol} →")));
            frame.render_stateful_widget(
                List::new(rows)
                    .block(Block::bordered().title("Edit provider"))
                    .highlight_style(SELECTED),
                areas[0],
                &mut ListState::default().with_selected(Some(edit.focus)),
            );
        }
        Mode::Route(edit) => {
            let editor =
                Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).split(areas[0]);
            let rows = ["Name", "Model glob"]
                .iter()
                .enumerate()
                .map(|(i, label)| field(label, &edit.fields[i], editor[0].width, edit.focus == i))
                .collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(rows)
                    .block(Block::bordered().title("Edit route"))
                    .highlight_style(SELECTED),
                editor[0],
                &mut ListState::default().with_selected((edit.focus < 2).then_some(edit.focus)),
            );
            let providers: Vec<_> = app
                .config
                .providers
                .iter()
                .map(|p| {
                    ListItem::new(format!(
                        "{} ({}) · {}",
                        p.name,
                        p.id,
                        protocol_label(&p.protocol)
                    ))
                })
                .collect();
            frame.render_stateful_widget(
                List::new(providers)
                    .block(Block::bordered().title("Provider · Tab to focus · ↑/↓ choose"))
                    .highlight_symbol("> ")
                    .highlight_style(if edit.focus == 2 {
                        SELECTED
                    } else {
                        Style::new().fg(Color::Gray)
                    }),
                editor[1],
                &mut edit.provider,
            );
        }
        Mode::Browse | Mode::Delete => draw_lists(frame, areas[0], app),
    }
    frame.render_widget(
        Paragraph::new(if app.restart_required {
            "Saved changes require a service restart."
        } else {
            "Service configuration: startup snapshot."
        })
        .style(Style::new().fg(if app.restart_required {
            Color::Yellow
        } else {
            Color::Gray
        })),
        areas[1],
    );
    let hints = match &app.mode {
        Mode::Provider(edit) => format!(
            "Tab/Shift-Tab fields · Ctrl-S save · Esc cancel\n{}",
            if edit.focus == 4 { "←/→ protocol" } else { "←/→ cursor · Home/End · Ctrl-U clear before cursor · paste text" }
        ),
        Mode::Route(edit) => format!(
            "Tab/Shift-Tab fields · Ctrl-S save · Esc cancel\n{}",
            if edit.focus == 2 { "↑/↓ or ←/→ choose provider" } else { "←/→ cursor · Home/End · Ctrl-U clear before cursor · paste text" }
        ),
        Mode::Delete => {
            let name = if app.routes_view {
                &app.config.routes[app.selected()].name
            } else {
                &app.config.providers[app.selected()].name
            };
            format!("Delete {} '{name}'?\ny confirm · any other key cancel",
                if app.routes_view { "route" } else { "provider" })
        }
        Mode::Browse if app.routes_view => "Tab providers · ↑/↓ select · a add · e edit · d delete\nK/J reorder · x clear default · v validate · q quit".into(),
        Mode::Browse => "Tab routes · ↑/↓ select · a add · e edit · d delete\nEnter default · v validate · q quit".into(),
    };
    let mut feedback = Text::from(hints);
    if !matches!(app.mode, Mode::Delete) {
        feedback.lines.push(Line::styled(
            &app.status,
            Style::new().fg(if app.status_error {
                Color::Red
            } else {
                Color::Green
            }),
        ));
    }
    frame.render_widget(
        Paragraph::new(feedback).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Keys and status"),
        ),
        areas[2],
    );
}

fn draw_lists(frame: &mut Frame, area: Rect, app: &mut App) {
    let areas = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(area);
    if app.routes_view {
        let rows = app.config.routes.iter().enumerate().map(|(i, r)| {
            Row::new(vec![
                (i + 1).to_string(),
                r.name.clone(),
                r.model_pattern.clone(),
                r.provider.clone(),
            ])
        });
        let default = default_provider(&app.config);
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(3),
                    Constraint::Fill(1),
                    Constraint::Fill(2),
                    Constraint::Fill(1),
                ],
            )
            .header(
                Row::new(["#", "Name", "Model glob", "Provider"])
                    .style(Style::new().fg(Color::Cyan)),
            )
            .block(Block::bordered().title(format!(
                "Routes · first match · default: {}",
                if default.is_empty() {
                    "(none)"
                } else {
                    default
                }
            )))
            .row_highlight_style(SELECTED)
            .highlight_symbol("> "),
            areas[0],
            &mut app.routes,
        );
        let detail = app.config.routes.get(app.selected()).map_or_else(
            || "No routes. Press a to add one.".into(),
            |r| format!("Model: {} → Provider: {}", r.model_pattern, r.provider),
        );
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title("Selected route")),
            areas[1],
        );
    } else {
        let rows = app.config.providers.iter().map(|p| {
            Row::new(vec![
                if p.id == default_provider(&app.config) {
                    "*".into()
                } else {
                    String::new()
                },
                p.name.clone(),
                p.id.clone(),
                protocol_label(&p.protocol).into(),
                if p.api_key.is_empty() {
                    "missing".into()
                } else {
                    "configured".into()
                },
            ])
        });
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(1),
                    Constraint::Fill(1),
                    Constraint::Fill(1),
                    Constraint::Length(16),
                    Constraint::Length(10),
                ],
            )
            .header(
                Row::new(["", "Name", "ID", "Protocol", "Key"]).style(Style::new().fg(Color::Cyan)),
            )
            .block(Block::bordered().title("Providers · * default route"))
            .row_highlight_style(SELECTED)
            .highlight_symbol("> "),
            areas[0],
            &mut app.providers,
        );
        let detail = app.config.providers.get(app.selected()).map_or_else(
            || "No providers. Press a to add the first provider.".into(),
            |p| format!("{} ({})\n{}", p.name, p.id, p.base_url),
        );
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title("Selected provider · address")),
            areas[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn config() -> crate::config::Config {
        serde_saphyr::from_str(include_str!("../../config.example.yaml")).unwrap()
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn long_unicode_input_keeps_cursor_visible_at_end_and_in_middle() {
        let mut input = TextInput::new("https://example.com/中文/模型");
        assert_eq!(input_value(&input, 7, true), "/模型▏");
        input.cursor = "https://".len();
        assert_eq!(input_value(&input, 7, true), "tps://▏");
        input.cursor = 0;
        assert_eq!(input_value(&input, 7, true), "▏https:");
        assert_eq!(input_value(&input, 1, true), "▏");
        assert_eq!(input_value(&input, 0, true), "");
    }

    #[test]
    fn lists_retain_selection_and_scroll_when_switching_views() {
        let mut config = config();
        for i in 0..30 {
            let mut provider = config.providers[0].clone();
            provider.id = format!("extra-{i}");
            provider.name = format!("Extra {i}");
            config.providers.push(provider);
        }
        let mut app = App::new(config);
        app.select(30);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let offset = app.providers.offset();
        assert!(offset > 0);
        assert!(screen(&terminal).contains("Extra 27"));
        app.routes_view = true;
        app.select(1);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        app.routes_view = false;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.providers.selected(), Some(30));
        assert_eq!(app.providers.offset(), offset);
        assert_eq!(app.routes.selected(), Some(1));
    }

    #[test]
    fn editor_shows_long_input_cursor_context_keys_and_masks_credentials() {
        let mut app = App::new(config());
        let mut editor = super::super::Editor::new(Some((0, &app.config.providers[0])));
        editor.fields[2] = TextInput::new(format!(
            "https://example.com/{}tailMarker",
            "segment/".repeat(30)
        ));
        editor.fields[3] = TextInput::new("must-never-appear");
        editor.focus = 2;
        app.mode = Mode::Provider(editor);
        app.restart_required = true;
        app.status_error = true;
        app.status = format!("Not saved: {} fieldErrorEnd", "invalid field ".repeat(8));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let content = screen(&terminal);
        assert!(content.contains("tailMarker▏"));
        assert!(content.contains("Ctrl-S save"));
        assert!(!content.contains("q quit"));
        assert!(content.contains("fieldErrorEnd"));
        assert!(content.contains("Saved changes require a service restart."));
        assert!(!content.contains("must-never-appear"));
    }
}

//! Provider configuration editor. Terminal restoration, key editing and bounded
//! list navigation adapted from cc-switch-cli (MIT, Jason Young / saladday).
//! Full upstream license accompanies the copied text editor in tui/text_edit.rs.
mod text_edit;

use crate::config::{Config, DefaultRoute, Protocol, Provider, Route, RouteRule, Server};
use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::{io, path::Path, sync::Arc};
use text_edit::TextInput;

type PanicHook = Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;
struct TerminalGuard {
    previous: PanicHook,
}
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
}
impl TerminalGuard {
    fn enter() -> Result<Self> {
        let previous: PanicHook = std::panic::take_hook().into();
        let hook = previous.clone();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        let guard = Self { previous };
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
        // Setting a hook while unwinding would panic again; the installed hook
        // already restored the terminal in that case.
        if !std::thread::panicking() {
            let previous = self.previous.clone();
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

struct Editor {
    index: Option<usize>,
    fields: [TextInput; 4],
    protocol: usize,
    focus: usize,
}
impl Editor {
    fn new(provider: Option<(usize, &Provider)>) -> Self {
        if let Some((i, p)) = provider {
            Self {
                index: Some(i),
                fields: [
                    TextInput::new(&p.id),
                    TextInput::new(&p.name),
                    TextInput::new(&p.base_url),
                    TextInput::new(&p.api_key),
                ],
                protocol: match p.protocol {
                    Protocol::Responses => 0,
                    Protocol::ChatCompletions => 1,
                    Protocol::Messages => 2,
                },
                focus: 0,
            }
        } else {
            Self {
                index: None,
                fields: std::array::from_fn(|_| TextInput::new("")),
                protocol: 0,
                focus: 0,
            }
        }
    }
    fn provider(&self) -> Provider {
        Provider {
            id: self.fields[0].value.clone(),
            name: self.fields[1].value.clone(),
            base_url: self.fields[2].value.clone(),
            api_key: self.fields[3].value.clone(),
            compat: Default::default(),
            protocol: match self.protocol {
                0 => Protocol::Responses,
                1 => Protocol::ChatCompletions,
                _ => Protocol::Messages,
            },
        }
    }
    fn apply(&self, config: &Config) -> Config {
        let mut next = config.clone();
        let provider = self.provider();
        if let Some(i) = self.index {
            if next.route.provider == next.providers[i].id {
                next.route.provider = provider.id.clone();
            }
            for route in &mut next.routes {
                if route.provider == next.providers[i].id {
                    route.provider = provider.id.clone();
                }
            }
            if let Some(default) = &mut next.default_route {
                if default.provider == next.providers[i].id {
                    default.provider = provider.id.clone();
                }
            }
            let mut provider = provider;
            provider.compat = next.providers[i].compat.clone();
            next.providers[i] = provider;
        } else {
            if next.providers.is_empty() {
                next.default_route = Some(DefaultRoute {
                    provider: provider.id.clone(),
                });
            }
            next.providers.push(provider);
        }
        next
    }
}

struct RouteEditor {
    index: Option<usize>,
    fields: [TextInput; 3],
    focus: usize,
}
impl RouteEditor {
    fn new(route: Option<(usize, &RouteRule)>) -> Self {
        Self {
            index: route.map(|(i, _)| i),
            fields: route.map_or_else(
                || std::array::from_fn(|_| TextInput::new("")),
                |(_, r)| {
                    [
                        TextInput::new(&r.name),
                        TextInput::new(&r.model_pattern),
                        TextInput::new(&r.provider),
                    ]
                },
            ),
            focus: 0,
        }
    }
    fn apply(&self, config: &Config) -> Config {
        let mut next = config.clone();
        let rule = RouteRule {
            name: self.fields[0].value.clone(),
            model_pattern: self.fields[1].value.clone(),
            provider: self.fields[2].value.clone(),
        };
        if let Some(i) = self.index {
            next.routes[i] = rule;
        } else {
            next.routes.push(rule);
        }
        next
    }
}

fn provider_is_referenced(config: &Config, id: &str) -> bool {
    config.route.provider == id
        || config
            .default_route
            .as_ref()
            .is_some_and(|r| r.provider == id)
        || config.routes.iter().any(|r| r.provider == id)
}

fn default_provider(config: &Config) -> &str {
    config
        .default_route
        .as_ref()
        .map_or(config.route.provider.as_str(), |r| r.provider.as_str())
}
fn protocol_label(p: &Protocol) -> &'static str {
    match p {
        Protocol::Responses => "responses",
        Protocol::ChatCompletions => "chat_completions",
        Protocol::Messages => "messages",
    }
}
fn save(next: Config, config: &mut Config, path: &Path, status: &mut String) -> bool {
    match next.validate().and_then(|_| next.save(path)) {
        Ok(()) => {
            *config = next;
            *status = "Saved and valid. Restart the service to apply changes.".into();
            true
        }
        Err(error) => {
            *status = format!("Not saved: {error}");
            false
        }
    }
}

pub fn run(path: &Path) -> Result<()> {
    let mut config = if path.try_exists()? {
        Config::load(path)?
    } else {
        Config {
            version: 1,
            compat: Default::default(),
            server: Server {
                listen: "0.0.0.0:15721".parse()?,
            },
            route: Route {
                provider: String::new(),
            },
            routes: vec![],
            default_route: None,
            providers: vec![],
        }
    };
    let mut selected = 0;
    let mut editor: Option<Editor> = None;
    let mut route_editor: Option<RouteEditor> = None;
    let mut routes_view = false;
    let mut delete_confirmation = false;
    let mut status = if config.providers.is_empty() {
        "New configuration: press a to add the first provider."
    } else {
        "Configuration valid. Running services use their startup snapshot."
    }
    .to_string();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        terminal.draw(|frame| {
            let areas =
                Layout::vertical([Constraint::Min(8), Constraint::Length(4)]).split(frame.area());
            if let Some(edit) = &route_editor {
                let rows: Vec<ListItem> = ["Name", "Model glob", "Provider ID"].iter().enumerate().map(|(i, label)| {
                    let input = &edit.fields[i];
                    let value = if i == edit.focus {
                        format!("{}▏{}", &input.value[..input.cursor], &input.value[input.cursor..])
                    } else { input.value.clone() };
                    ListItem::new(format!("{label}: {value}"))
                }).collect();
                let mut state = ListState::default().with_selected(Some(edit.focus));
                frame.render_stateful_widget(
                    List::new(rows).block(Block::default().borders(Borders::ALL).title("Edit route: Tab fields · Ctrl-S save · Esc cancel"))
                        .highlight_style(Style::default().fg(Color::Yellow)), areas[0], &mut state,
                );
            } else if let Some(edit) = &editor {
                let labels = ["ID", "Name", "Base URL", "API key", "Protocol"];
                let rows: Vec<ListItem> = labels
                    .iter()
                    .enumerate()
                    .map(|(i, label)| {
                        let value = if i == 4 {
                            protocol_label(&edit.provider().protocol).to_string()
                        } else if i == 3 {
                            if edit.fields[3].value.is_empty() {
                                "(missing)".into()
                            } else {
                                "******** (configured)".into()
                            }
                        } else {
                            let input = &edit.fields[i];
                            if i == edit.focus {
                                format!(
                                    "{}▏{}",
                                    &input.value[..input.cursor],
                                    &input.value[input.cursor..]
                                )
                            } else {
                                input.value.clone()
                            }
                        };
                        ListItem::new(format!("{label}: {value}"))
                    })
                    .collect();
                let mut state = ListState::default().with_selected(Some(edit.focus));
                frame.render_stateful_widget(
                    List::new(rows)
                        .block(Block::default().borders(Borders::ALL).title(
                            "Edit provider: Tab fields · ←/→ protocol · Ctrl-S save · Esc cancel",
                        ))
                        .highlight_style(Style::default().fg(Color::Yellow)),
                    areas[0],
                    &mut state,
                );
            } else if routes_view {
                let rows: Vec<ListItem> = config.routes.iter().enumerate().map(|(i, r)| {
                    ListItem::new(format!("{}. {} | {} → {}", i + 1, r.name, r.model_pattern, r.provider))
                }).collect();
                let mut state = ListState::default().with_selected(if rows.is_empty() { None } else { Some(selected) });
                frame.render_stateful_widget(
                    List::new(rows).block(Block::default().borders(Borders::ALL).title(format!("Routes · first match · default: {}", default_provider(&config))))
                        .highlight_style(Style::default().fg(Color::Yellow)).highlight_symbol("> "), areas[0], &mut state,
                );
            } else {
                let items: Vec<ListItem> = config
                    .providers
                    .iter()
                    .map(|p| {
                        ListItem::new(format!(
                            "{} {} ({}) | {} | {} | key: {}",
                            if p.id == default_provider(&config) {
                                "*"
                            } else {
                                " "
                            },
                            p.name,
                            p.id,
                            protocol_label(&p.protocol),
                            p.base_url,
                            if p.api_key.is_empty() {
                                "missing"
                            } else {
                                "configured"
                            }
                        ))
                    })
                    .collect();
                let mut state = ListState::default().with_selected(if items.is_empty() {
                    None
                } else {
                    Some(selected)
                });
                frame.render_stateful_widget(
                    List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title("Providers · * default route"),
                        )
                        .highlight_style(Style::default().fg(Color::Yellow))
                        .highlight_symbol("> "),
                    areas[0],
                    &mut state,
                );
            }
            frame.render_widget(
                Paragraph::new(format!(
                    "{}\n{status}",
                    if routes_view { "Tab providers · a add · e edit · d delete · K/J reorder · x clear default · v validate · q quit" }
                    else { "Tab routes · a add · e edit · d delete · Enter default · v validate · q quit" }
                ))
                .block(Block::default().borders(Borders::ALL)),
                areas[1],
            );
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
        }
        if let Some(edit) = editor.as_mut() {
            match key.code {
                KeyCode::Esc => editor = None,
                KeyCode::Tab => edit.focus = (edit.focus + 1) % 5,
                KeyCode::BackTab => edit.focus = (edit.focus + 4) % 5,
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let next = edit.apply(&config);
                    if save(next, &mut config, path, &mut status) {
                        editor = None;
                    }
                }
                KeyCode::Left if edit.focus == 4 => edit.protocol = (edit.protocol + 2) % 3,
                KeyCode::Right | KeyCode::Char(' ') if edit.focus == 4 => {
                    edit.protocol = (edit.protocol + 1) % 3
                }
                _ if edit.focus < 4 => {
                    edit.fields[edit.focus].apply_key(key);
                }
                _ => {}
            }
            continue;
        }
        if let Some(edit) = route_editor.as_mut() {
            match key.code {
                KeyCode::Esc => route_editor = None,
                KeyCode::Tab => edit.focus = (edit.focus + 1) % 3,
                KeyCode::BackTab => edit.focus = (edit.focus + 2) % 3,
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if save(edit.apply(&config), &mut config, path, &mut status) {
                        route_editor = None;
                    }
                }
                _ => {
                    edit.fields[edit.focus].apply_key(key);
                }
            }
            continue;
        }
        if delete_confirmation {
            delete_confirmation = false;
            if key.code == KeyCode::Char('y') {
                let mut next = config.clone();
                if routes_view {
                    next.routes.remove(selected);
                } else {
                    next.providers.remove(selected);
                }
                if save(next, &mut config, path, &mut status) {
                    let len = if routes_view {
                        config.routes.len()
                    } else {
                        config.providers.len()
                    };
                    selected = selected.min(len.saturating_sub(1));
                }
            } else {
                status = "Deletion cancelled.".into();
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Tab => {
                routes_view = !routes_view;
                selected = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                let len = if routes_view {
                    config.routes.len()
                } else {
                    config.providers.len()
                };
                selected = (selected + 1).min(len.saturating_sub(1));
            }
            KeyCode::Char('a') if routes_view => route_editor = Some(RouteEditor::new(None)),
            KeyCode::Char('e') if routes_view => {
                if let Some(r) = config.routes.get(selected) {
                    route_editor = Some(RouteEditor::new(Some((selected, r))));
                }
            }
            KeyCode::Char('d') if routes_view => {
                if !config.routes.is_empty() {
                    delete_confirmation = true;
                    status = "Delete selected route? y confirms; any other key cancels.".into();
                }
            }
            KeyCode::Char('K' | 'J') if routes_view && !config.routes.is_empty() => {
                let target = if key.code == KeyCode::Char('K') {
                    selected.saturating_sub(1)
                } else {
                    (selected + 1).min(config.routes.len() - 1)
                };
                if target != selected {
                    let mut next = config.clone();
                    next.routes.swap(selected, target);
                    if save(next, &mut config, path, &mut status) {
                        selected = target;
                    }
                }
            }
            KeyCode::Char('x') if routes_view => {
                let mut next = config.clone();
                next.default_route = None;
                next.route = Route::default();
                save(next, &mut config, path, &mut status);
            }
            KeyCode::Char('a') => editor = Some(Editor::new(None)),
            KeyCode::Char('e') => {
                if let Some(p) = config.providers.get(selected) {
                    editor = Some(Editor::new(Some((selected, p))));
                }
            }
            KeyCode::Enter if !routes_view => {
                if let Some(p) = config.providers.get(selected) {
                    let mut next = config.clone();
                    next.default_route = Some(DefaultRoute {
                        provider: p.id.clone(),
                    });
                    next.route = Route::default();
                    save(next, &mut config, path, &mut status);
                }
            }
            KeyCode::Char('d') => {
                if let Some(p) = config.providers.get(selected) {
                    if provider_is_referenced(&config, &p.id) {
                        status =
                            "Change routes referencing this provider before deleting it.".into();
                    } else {
                        delete_confirmation = true;
                        status =
                            "Delete selected provider? y confirms; any other key cancels.".into();
                    }
                }
            }
            KeyCode::Char('v') => {
                status = match config.validate() {
                    Ok(()) => {
                        "Configuration valid. Restart the service after saved changes.".into()
                    }
                    Err(error) => format!("Invalid: {error}"),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editing_current_provider_renames_route_without_mutating_original() {
        let mut config = Config {
            version: 1,
            compat: Default::default(),
            server: Server {
                listen: "127.0.0.1:15721".parse().unwrap(),
            },
            route: Route {
                provider: "old".into(),
            },
            routes: vec![],
            default_route: None,
            providers: vec![Provider {
                id: "old".into(),
                name: "Provider".into(),
                protocol: Protocol::Responses,
                base_url: "https://example.com/v1".into(),
                api_key: "secret".into(),
                compat: Default::default(),
            }],
        };
        config.providers[0].compat.meta.max_output_tokens = Some(16384);
        config.providers[0].compat.settings_config = serde_json::json!({"modelCatalog":{"models":[{"model":"example","modalities":{"input":["text"]}}]}});
        config.compat.optimizer.cache_injection = false;
        config.routes.push(RouteRule {
            name: "claude".into(),
            model_pattern: "claude-*".into(),
            provider: "old".into(),
        });
        config.default_route = Some(DefaultRoute {
            provider: "old".into(),
        });
        let mut editor = Editor::new(Some((0, &config.providers[0])));
        editor.fields[0] = TextInput::new("new");
        let next = editor.apply(&config);
        assert_eq!(next.route.provider, "new");
        assert_eq!(next.routes[0].provider, "new");
        assert_eq!(next.default_route.as_ref().unwrap().provider, "new");
        assert!(provider_is_referenced(&next, "new"));
        next.validate().unwrap();
        assert_eq!(next.providers[0].id, "new");
        assert_eq!(config.route.provider, "old");
        assert_eq!(next.providers[0].api_key, "secret");
        assert_eq!(next.providers[0].compat.meta.max_output_tokens, Some(16384));
        assert_eq!(
            next.providers[0].compat.settings_config,
            config.providers[0].compat.settings_config
        );
        assert!(!next.compat.optimizer.cache_injection);
    }

    #[test]
    fn route_editor_rejects_missing_provider_without_changing_config() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let mut editor = RouteEditor::new(None);
        editor.fields = [
            TextInput::new("new"),
            TextInput::new("gpt-*"),
            TextInput::new("missing"),
        ];
        assert!(editor.apply(&config).validate().is_err());
        editor.fields[2] = TextInput::new(&config.providers[0].id);
        let next = editor.apply(&config);
        next.validate().unwrap();
        assert_eq!(next.routes.len(), config.routes.len() + 1);
    }
}

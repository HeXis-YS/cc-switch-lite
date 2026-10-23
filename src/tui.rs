//! Provider configuration editor. Terminal restoration, key editing and bounded
//! list navigation adapted from cc-switch-cli (MIT, Jason Young / saladday).
//! Full upstream license accompanies the copied text editor in tui/text_edit.rs.
mod text_edit;

use crate::config::{Config, Protocol, Provider, Route, Server};
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
            let mut provider = provider;
            provider.compat = next.providers[i].compat.clone();
            next.providers[i] = provider;
        } else {
            if next.providers.is_empty() {
                next.route.provider = provider.id.clone();
            }
            next.providers.push(provider);
        }
        next
    }
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
            if let Some(edit) = &editor {
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
            } else {
                let items: Vec<ListItem> = config
                    .providers
                    .iter()
                    .map(|p| {
                        ListItem::new(format!(
                            "{} {} ({}) | {} | {} | key: {}",
                            if p.id == config.route.provider {
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
                                .title("Providers · * current route"),
                        )
                        .highlight_style(Style::default().fg(Color::Yellow))
                        .highlight_symbol("> "),
                    areas[0],
                    &mut state,
                );
            }
            frame.render_widget(
                Paragraph::new(format!(
                    "a add · e edit · d delete · Enter select · v validate · q quit\n{status}"
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
        if delete_confirmation {
            delete_confirmation = false;
            if key.code == KeyCode::Char('y') {
                let mut next = config.clone();
                next.providers.remove(selected);
                if save(next, &mut config, path, &mut status) {
                    selected = selected.min(config.providers.len().saturating_sub(1));
                }
            } else {
                status = "Deletion cancelled.".into();
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(config.providers.len().saturating_sub(1))
            }
            KeyCode::Char('a') => editor = Some(Editor::new(None)),
            KeyCode::Char('e') => {
                if let Some(p) = config.providers.get(selected) {
                    editor = Some(Editor::new(Some((selected, p))));
                }
            }
            KeyCode::Enter => {
                if let Some(p) = config.providers.get(selected) {
                    let mut next = config.clone();
                    next.route.provider = p.id.clone();
                    save(next, &mut config, path, &mut status);
                }
            }
            KeyCode::Char('d') => {
                if let Some(p) = config.providers.get(selected) {
                    if p.id == config.route.provider {
                        status =
                            "Select another route before deleting the current provider.".into();
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
        let mut editor = Editor::new(Some((0, &config.providers[0])));
        editor.fields[0] = TextInput::new("new");
        let next = editor.apply(&config);
        assert_eq!(next.route.provider, "new");
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
}

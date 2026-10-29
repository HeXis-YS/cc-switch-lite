//! Provider configuration editor. Terminal restoration, key editing and bounded
//! list navigation adapted from cc-switch-cli (MIT, Jason Young / saladday).
//! Full upstream license accompanies the copied text editor in tui/text_edit.rs.
mod text_edit;
mod view;

use crate::config::{Config, DefaultRoute, Protocol, Provider, Route, RouteRule, Server};
use anyhow::Result;
use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    widgets::{ListState, TableState},
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
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        cursor::Show,
        LeaveAlternateScreen
    );
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
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            cursor::Hide
        )?;
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
    fields: [TextInput; 2],
    provider: ListState,
    focus: usize,
}
impl RouteEditor {
    fn new(route: Option<(usize, &RouteRule)>, config: &Config, selected: usize) -> Self {
        let provider = route
            .and_then(|(_, r)| config.providers.iter().position(|p| p.id == r.provider))
            .unwrap_or(selected);
        Self {
            index: route.map(|(i, _)| i),
            fields: route.map_or_else(
                || std::array::from_fn(|_| TextInput::new("")),
                |(_, r)| [TextInput::new(&r.name), TextInput::new(&r.model_pattern)],
            ),
            provider: ListState::default().with_selected(Some(provider)),
            focus: 0,
        }
    }
    fn apply(&self, config: &Config) -> Config {
        let mut next = config.clone();
        let rule = RouteRule {
            name: self.fields[0].value.clone(),
            model_pattern: self.fields[1].value.clone(),
            provider: config.providers[self.provider.selected().unwrap_or(0)]
                .id
                .clone(),
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
enum Mode {
    Browse,
    Provider(Editor),
    Route(RouteEditor),
    Delete,
}

struct App {
    config: Config,
    mode: Mode,
    routes_view: bool,
    providers: TableState,
    routes: TableState,
    status: String,
    status_error: bool,
    restart_required: bool,
}

impl App {
    fn new(config: Config) -> Self {
        let status = if config.providers.is_empty() {
            "New configuration: press a to add the first provider."
        } else {
            "Configuration valid. Running services use their startup snapshot."
        }
        .to_string();
        let providers =
            TableState::default().with_selected((!config.providers.is_empty()).then_some(0));
        let routes = TableState::default().with_selected((!config.routes.is_empty()).then_some(0));
        Self {
            config,
            mode: Mode::Browse,
            routes_view: false,
            providers,
            routes,
            status,
            status_error: false,
            restart_required: false,
        }
    }

    fn selected(&self) -> usize {
        if self.routes_view {
            &self.routes
        } else {
            &self.providers
        }
        .selected()
        .unwrap_or(0)
    }

    fn select(&mut self, index: usize) {
        let (state, len) = if self.routes_view {
            (&mut self.routes, self.config.routes.len())
        } else {
            (&mut self.providers, self.config.providers.len())
        };
        state.select((len > 0).then_some(index.min(len.saturating_sub(1))));
    }

    fn save(&mut self, next: Config, path: &Path) -> bool {
        match next.save(path) {
            Ok(()) => {
                self.config = next;
                self.status = "Saved and valid.".into();
                self.status_error = false;
                self.restart_required = true;
                true
            }
            Err(error) => {
                self.status = format!("Not saved: {error}");
                self.status_error = true;
                false
            }
        }
    }

    fn paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Provider(edit) if edit.focus < 4 => {
                edit.fields[edit.focus].insert_paste(text);
            }
            Mode::Route(edit) if edit.focus < 2 => {
                edit.fields[edit.focus].insert_paste(text);
            }
            _ => {}
        }
    }

    // Return false only when the user exits; editor and confirmation keys never
    // fall through to list actions.
    fn key(&mut self, key: KeyEvent, path: &Path) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return false;
        }
        let save_key =
            key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.mode {
            Mode::Provider(edit) => {
                if save_key {
                    let next = edit.apply(&self.config);
                    let selected = edit.index.unwrap_or(self.config.providers.len());
                    if self.save(next, path) {
                        self.providers.select(Some(selected));
                        self.mode = Mode::Browse;
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => self.mode = Mode::Browse,
                        KeyCode::Tab => edit.focus = (edit.focus + 1) % 5,
                        KeyCode::BackTab => edit.focus = (edit.focus + 4) % 5,
                        KeyCode::Left if edit.focus == 4 => edit.protocol = (edit.protocol + 2) % 3,
                        KeyCode::Right | KeyCode::Char(' ') if edit.focus == 4 => {
                            edit.protocol = (edit.protocol + 1) % 3;
                        }
                        _ if edit.focus < 4 => {
                            edit.fields[edit.focus].apply_key(key);
                        }
                        _ => {}
                    }
                }
                return true;
            }
            Mode::Route(edit) => {
                if save_key {
                    let next = edit.apply(&self.config);
                    let selected = edit.index.unwrap_or(self.config.routes.len());
                    if self.save(next, path) {
                        self.routes.select(Some(selected));
                        self.mode = Mode::Browse;
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => self.mode = Mode::Browse,
                        KeyCode::Tab => edit.focus = (edit.focus + 1) % 3,
                        KeyCode::BackTab => edit.focus = (edit.focus + 2) % 3,
                        KeyCode::Up | KeyCode::Left if edit.focus == 2 => {
                            let selected = edit.provider.selected().unwrap_or(0);
                            edit.provider.select(Some(selected.saturating_sub(1)));
                        }
                        KeyCode::Down | KeyCode::Right if edit.focus == 2 => {
                            let selected = edit.provider.selected().unwrap_or(0);
                            edit.provider
                                .select(Some((selected + 1).min(self.config.providers.len() - 1)));
                        }
                        _ if edit.focus < 2 => {
                            edit.fields[edit.focus].apply_key(key);
                        }
                        _ => {}
                    }
                }
                return true;
            }
            Mode::Delete => {
                self.mode = Mode::Browse;
                if key.code == KeyCode::Char('y') {
                    let selected = self.selected();
                    let mut next = self.config.clone();
                    if self.routes_view {
                        next.routes.remove(selected);
                    } else {
                        next.providers.remove(selected);
                    }
                    if self.save(next, path) {
                        self.select(selected);
                    }
                } else {
                    self.status = "Deletion cancelled.".into();
                    self.status_error = false;
                }
                return true;
            }
            Mode::Browse => {}
        }

        let selected = self.selected();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Tab | KeyCode::BackTab => self.routes_view = !self.routes_view,
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::Char('a' | 'e') if self.routes_view => {
                if self.config.providers.is_empty() {
                    self.status = "Add a provider before creating a route.".into();
                    self.status_error = true;
                } else if key.code == KeyCode::Char('a') {
                    self.mode = Mode::Route(RouteEditor::new(
                        None,
                        &self.config,
                        self.providers.selected().unwrap_or(0),
                    ));
                } else if let Some(route) = self.config.routes.get(selected) {
                    self.mode =
                        Mode::Route(RouteEditor::new(Some((selected, route)), &self.config, 0));
                }
            }
            KeyCode::Char('a') => self.mode = Mode::Provider(Editor::new(None)),
            KeyCode::Char('e') => {
                if let Some(provider) = self.config.providers.get(selected) {
                    self.mode = Mode::Provider(Editor::new(Some((selected, provider))));
                }
            }
            KeyCode::Char('d') => {
                if self.routes_view {
                    if !self.config.routes.is_empty() {
                        self.mode = Mode::Delete;
                    }
                } else if let Some(provider) = self.config.providers.get(selected) {
                    if provider_is_referenced(&self.config, &provider.id) {
                        let mut references: Vec<&str> = self
                            .config
                            .routes
                            .iter()
                            .filter(|r| r.provider == provider.id)
                            .map(|r| r.name.as_str())
                            .collect();
                        if self
                            .config
                            .default_route
                            .as_ref()
                            .is_some_and(|r| r.provider == provider.id)
                        {
                            references.push("default route");
                        }
                        if self.config.route.provider == provider.id {
                            references.push("legacy route");
                        }
                        self.status = format!(
                            "Cannot delete '{}': change its references first: {}.",
                            provider.name,
                            references.join(", "),
                        );
                        self.status_error = true;
                    } else {
                        self.mode = Mode::Delete;
                    }
                }
            }
            KeyCode::Char('K' | 'J') if self.routes_view && !self.config.routes.is_empty() => {
                let target = if key.code == KeyCode::Char('K') {
                    selected.saturating_sub(1)
                } else {
                    (selected + 1).min(self.config.routes.len() - 1)
                };
                if target != selected {
                    let mut next = self.config.clone();
                    next.routes.swap(selected, target);
                    if self.save(next, path) {
                        self.select(target);
                    }
                }
            }
            KeyCode::Char('x') if self.routes_view => {
                let mut next = self.config.clone();
                next.default_route = None;
                next.route = Route::default();
                self.save(next, path);
            }
            KeyCode::Enter if !self.routes_view => {
                if let Some(provider) = self.config.providers.get(selected) {
                    let mut next = self.config.clone();
                    next.default_route = Some(DefaultRoute {
                        provider: provider.id.clone(),
                    });
                    next.route = Route::default();
                    self.save(next, path);
                }
            }
            KeyCode::Char('v') => {
                self.status_error = false;
                self.status = match self.config.validate() {
                    Ok(()) => "Configuration valid.".into(),
                    Err(error) => {
                        self.status_error = true;
                        format!("Invalid: {error}")
                    }
                };
            }
            _ => {}
        }
        true
    }
}

pub fn run(path: &Path) -> Result<()> {
    let config = if path.try_exists()? {
        Config::load(path)?
    } else {
        Config {
            version: 1,
            compat: Default::default(),
            server: Server {
                listen: "0.0.0.0:15721".parse()?,
            },
            route: Route::default(),
            routes: vec![],
            default_route: None,
            providers: vec![],
        }
    };
    let mut app = App::new(config);
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        terminal.draw(|frame| view::draw(frame, &mut app))?;
        match event::read()? {
            Event::Key(key) if !app.key(key, path) => break,
            Event::Paste(text) => app.paste(&text),
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
    fn route_editor_uses_selected_provider_and_restores_existing_reference() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let mut editor = RouteEditor::new(None, &config, 1);
        editor.fields = [TextInput::new("new"), TextInput::new("gpt-*")];
        let next = editor.apply(&config);
        next.validate().unwrap();
        assert_eq!(next.routes.len(), config.routes.len() + 1);
        assert_eq!(next.routes.last().unwrap().provider, config.providers[1].id);
        let editor = RouteEditor::new(Some((0, &config.routes[0])), &config, 0);
        assert_eq!(
            editor.apply(&config).routes[0].provider,
            config.routes[0].provider
        );
    }

    #[test]
    fn failed_save_preserves_draft_and_success_requires_restart_after_validation() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let mut app = App::new(config);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        app.key(key(KeyCode::Char('a'), KeyModifiers::NONE), &path);
        app.paste("duplicate");
        app.key(key(KeyCode::Char('s'), KeyModifiers::CONTROL), &path);
        assert!(matches!(app.mode, Mode::Provider(_)));
        assert!(app.status_error);
        assert!(!path.exists());
        assert_eq!(app.config.providers.len(), 3);
        if let Mode::Provider(edit) = &mut app.mode {
            edit.fields[1] = TextInput::new("Added provider");
            edit.fields[2] = TextInput::new("https://example.net/v1");
            edit.fields[3] = TextInput::new("private-key");
        }
        app.key(key(KeyCode::Char('s'), KeyModifiers::CONTROL), &path);
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(Config::load(&path).unwrap().providers.len(), 4);
        app.key(key(KeyCode::Char('v'), KeyModifiers::NONE), &path);
        assert!(app.restart_required);
        assert!(!app.status_error);
        // Pasting list shortcuts must not execute actions outside a text field.
        app.paste("dyaq");
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(app.config.providers.len(), 4);
    }
}

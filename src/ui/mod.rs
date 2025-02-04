use std::{collections::HashMap, io::Stdout, ops::AddAssign, time::Duration};

#[cfg(debug_assertions)]
use crate::config::tabs::PaneType;
use anyhow::{anyhow, Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use enum_map::{enum_map, Enum, EnumMap};
use itertools::Itertools;
use modals::{decoders::DecodersModal, keybinds::KeybindsModal, outputs::OutputsModal, song_info::SongInfoModal};
use panes::{PaneContainer, Panes};
#[cfg(debug_assertions)]
use ratatui::style::Stylize;

use ratatui::{
    layout::Rect,
    prelude::{Backend, Constraint, CrosstermBackend, Layout},
    style::{Color, Style},
    symbols::border,
    text::Text,
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};
use tab_screen::TabScreen;
use widgets::app_tabs::AppTabs;

use crate::{
    cli::{create_env, run_external},
    config::{
        cli::Args,
        keys::{CommonAction, GlobalAction},
        tabs::TabName,
        Config,
    },
    mpd::{
        client::Client,
        commands::{idle::IdleEvent, volume::Bound, Song, State},
        mpd_client::{FilterKind, MpdClient, ValueChange},
    },
    shared::{
        key_event::KeyEvent,
        macros::{modal, status_error, status_info, status_warn, try_ret},
        mouse_event::{MouseEvent, MouseEventKind},
    },
};
use crate::{context::AppContext, mpd::version::Version};

use self::{modals::Modal, panes::Pane, widgets::header::Header};

pub mod browser;
pub mod dirstack;
pub mod image;
pub mod modals;
pub mod panes;
pub mod tab_screen;
pub mod widgets;

#[derive(Debug)]
#[allow(dead_code)]
pub enum Level {
    Trace,
    Debug,
    Warn,
    Error,
    Info,
}

#[derive(Debug)]
pub struct StatusMessage {
    pub message: String,
    pub level: Level,
    pub created: std::time::Instant,
}

#[derive(Debug)]
pub struct Ui<'ui> {
    panes: PaneContainer,
    modals: Vec<Box<dyn Modal>>,
    status_message: Option<StatusMessage>,
    rendered_frames_count: u32,
    current_song: Option<Song>,
    command: Option<String>,
    active_tab: TabName,
    tabs: HashMap<TabName, TabScreen>,
    areas: EnumMap<Areas, Rect>,
    tab_bar: AppTabs<'ui>,
}

macro_rules! screen_call {
    ($self:ident, $fn:ident($($param:expr),+)) => {
        $self.tabs
            .get_mut(&$self.active_tab)
            .context(anyhow!("Expected tab '{}' to be defined. Please report this along with your config.", $self.active_tab))?
            .$fn(&mut $self.panes, $($param),+)
    }
}

#[derive(Debug, Enum)]
enum Areas {
    Header,
    Tabs,
    Content,
    Bar,
}

impl<'ui> Ui<'ui> {
    pub fn new(context: &AppContext) -> Result<Ui<'ui>> {
        let active_tab = *context.config.tabs.names.first().context("Expected at least one tab")?;
        Ok(Self {
            panes: PaneContainer::new(context),
            tab_bar: AppTabs::new(active_tab, context.config),
            status_message: None,
            rendered_frames_count: 0,
            current_song: None,
            modals: Vec::default(),
            command: None,
            active_tab,
            tabs: context
                .config
                .tabs
                .tabs
                .iter()
                .map(|(name, screen)| -> Result<_> { Ok((*name, TabScreen::new(&screen.panes))) })
                .try_collect()?,
            areas: enum_map! {
                _ => Rect::default()
            },
        })
    }

    fn calc_areas(&mut self, area: Rect, context: &AppContext) -> Result<()> {
        let tab_area_height = match (context.config.theme.tab_bar.enabled, context.config.theme.draw_borders) {
            (true, true) => 3,
            (true, false) => 1,
            (false, _) => 0,
        };

        let [header_area, tabs_area, content_area, bar_area] = *Layout::vertical([
            Constraint::Length(u16::try_from(context.config.theme.header.rows.len())?),
            Constraint::Length(tab_area_height), // Tab bar
            Constraint::Percentage(100),
            Constraint::Min(1),
        ])
        .split(area) else {
            return Ok(());
        };
        self.areas[Areas::Header] = header_area;
        self.areas[Areas::Tabs] = tabs_area;
        self.areas[Areas::Content] = content_area;
        self.areas[Areas::Bar] = bar_area;

        Ok(())
    }
    pub fn post_render(&mut self, frame: &mut Frame, context: &mut AppContext) -> Result<()> {
        screen_call!(self, post_render(frame, context))
    }

    fn change_tab(&mut self, new_tab: TabName, client: &mut impl MpdClient, context: &AppContext) -> Result<()> {
        screen_call!(self, on_hide(client, &context))?;
        self.active_tab = new_tab;
        screen_call!(self, before_show(client, &context))?;
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, context: &mut AppContext) -> Result<()> {
        self.calc_areas(frame.area(), context)?;

        if let Some(bg_color) = context.config.theme.background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), frame.area());
        }
        self.rendered_frames_count.add_assign(1);
        if self
            .status_message
            .as_ref()
            .is_some_and(|m| m.created.elapsed() > std::time::Duration::from_secs(5))
        {
            self.status_message = None;
        }

        let header = Header::new(context.config, &context.status, self.current_song.as_ref());
        frame.render_widget(header, self.areas[Areas::Header]);

        if self.areas[Areas::Tabs].height > 0 {
            self.tab_bar.set_selected(self.active_tab);
            self.tab_bar.render(self.areas[Areas::Tabs], frame.buffer_mut());
        }

        if let Some(command) = &self.command {
            let [leader_area, command_area] =
                *Layout::horizontal([Constraint::Length(1), Constraint::Percentage(100)]).split(self.areas[Areas::Bar])
            else {
                return Ok(());
            };

            let status_bar = Paragraph::new(command.as_str())
                .alignment(ratatui::prelude::Alignment::Left)
                .style(context.config.as_text_style());

            frame.render_widget(Text::from(":"), leader_area);
            frame.render_widget(status_bar, command_area);
        } else if let Some(StatusMessage { message, level, .. }) = &self.status_message {
            let status_bar = Paragraph::new(message.to_owned())
                .alignment(ratatui::prelude::Alignment::Center)
                .style(Style::default().fg(level.into()).bg(Color::Black));
            frame.render_widget(status_bar, self.areas[Areas::Bar]);
        } else if context.config.status_update_interval_ms.is_some() {
            let elapsed_bar = context.config.as_styled_progress_bar();
            let elapsed_bar = if context.status.duration == Duration::ZERO {
                elapsed_bar.value(0.0)
            } else {
                elapsed_bar.value(context.status.elapsed.as_secs_f32() / context.status.duration.as_secs_f32())
            };
            frame.render_widget(elapsed_bar, self.areas[Areas::Bar]);
        }

        #[cfg(debug_assertions)]
        #[allow(clippy::cast_possible_truncation)]
        {
            let text = format!("{} frames", self.rendered_frames_count);
            let mut area = self.areas[Areas::Bar];
            area.width = text.chars().count() as u16;
            frame.render_widget(
                Text::from(text)
                    .fg(context.config.theme.text_color.unwrap_or_default())
                    .bg(context.config.theme.background_color.unwrap_or_default()),
                area,
            );
        }

        screen_call!(self, render(frame, self.areas[Areas::Content], context))?;

        for modal in &mut self.modals {
            modal.render(frame, context)?;
        }

        Ok(())
    }

    pub fn handle_mouse_event(
        &mut self,
        event: MouseEvent,
        client: &mut Client<'_>,
        context: &mut AppContext,
    ) -> Result<()> {
        if let Some(ref mut modal) = self.modals.last_mut() {
            modal.handle_mouse_event(event, client, context)?;
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick if self.areas[Areas::Header].contains(event.into()) => {
                client.pause_toggle()?;
                context.render()?;
            }
            MouseEventKind::ScrollUp if self.areas[Areas::Header].contains(event.into()) => {
                client.set_volume(*context.status.volume.inc_by(context.config.volume_step))?;
                context.render()?;
            }
            MouseEventKind::ScrollDown if self.areas[Areas::Header].contains(event.into()) => {
                client.set_volume(*context.status.volume.dec_by(context.config.volume_step))?;
                context.render()?;
            }
            MouseEventKind::LeftClick if self.areas[Areas::Bar].contains(event.into()) => {
                if !matches!(context.status.state, State::Play | State::Pause) {
                    return Ok(());
                }

                let second_to_seek_to = context
                    .status
                    .duration
                    .mul_f32(f32::from(event.x) / f32::from(self.areas[Areas::Bar].width))
                    .as_secs();
                client.seek_current(ValueChange::Set(u32::try_from(second_to_seek_to)?))?;

                context.render()?;
            }
            MouseEventKind::ScrollDown if self.areas[Areas::Tabs].contains(event.into()) => {
                self.change_tab(context.config.next_screen(self.active_tab), client, context)?;
                context.render()?;
            }
            MouseEventKind::ScrollUp if self.areas[Areas::Tabs].contains(event.into()) => {
                self.change_tab(context.config.prev_screen(self.active_tab), client, context)?;
                context.render()?;
            }
            MouseEventKind::LeftClick if self.areas[Areas::Tabs].contains(event.into()) => {
                if let Some(tab_name) = self
                    .tab_bar
                    .get_tab_idx_at(event.into())
                    .and_then(|idx| context.config.tabs.names.get(idx))
                {
                    if &self.active_tab != tab_name {
                        self.change_tab(*tab_name, client, context)?;
                        context.render()?;
                        return Ok(());
                    }
                }
            }
            _ if self.areas[Areas::Content].contains(event.into()) => {
                screen_call!(self, handle_mouse_event(event, client, context))?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn handle_key(
        &mut self,
        key: &mut KeyEvent,
        context: &mut AppContext,
        client: &mut Client<'_>,
    ) -> Result<KeyHandleResult> {
        if let Some(ref mut command) = self.command {
            let action = key.as_common_action(context);
            if let Some(CommonAction::Close) = action {
                self.command = None;
                context.render()?;
                return Ok(KeyHandleResult::None);
            } else if let Some(CommonAction::Confirm) = action {
                let cmd = command.parse();
                log::debug!("Executing command: {:?}", cmd);

                self.command = None;
                match cmd {
                    Ok(Args { command: Some(cmd), .. }) => {
                        cmd.execute(client, context.config, |request, _| {
                            if let Err(err) = context.work_sender.send(request) {
                                status_error!("Failed to send work request: {}", err);
                            }
                        })?;
                    }
                    Err(err) => {
                        status_error!("Failed to parse command. {:?}", err);
                    }
                    _ => {}
                }

                context.render()?;
                return Ok(KeyHandleResult::None);
            }

            match key.code() {
                KeyCode::Char(c) => {
                    command.push(c);
                    context.render()?;
                }
                KeyCode::Backspace => {
                    command.pop();
                    context.render()?;
                }
                _ => {}
            }

            return Ok(KeyHandleResult::None);
        }

        if let Some(ref mut modal) = self.modals.last_mut() {
            modal.handle_key(key, client, context)?;
            return Ok(KeyHandleResult::None);
        }

        screen_call!(self, handle_action(key, client, context))?;

        if let Some(action) = key.as_global_action(context) {
            match action {
                GlobalAction::Command { command, .. } => {
                    let cmd = command.parse();
                    log::debug!("executing {:?}", cmd);

                    self.command = None;
                    if let Ok(Args { command: Some(cmd), .. }) = cmd {
                        cmd.execute(client, context.config, |request, _| {
                            if let Err(err) = context.work_sender.send(request) {
                                status_error!("Failed to send work request: {}", err);
                            }
                        })?;
                    }
                }
                GlobalAction::CommandMode => {
                    self.command = Some(String::new());
                    context.render()?;
                }
                GlobalAction::NextTrack if context.status.state == State::Play => client.next()?,
                GlobalAction::PreviousTrack if context.status.state == State::Play => client.prev()?,
                GlobalAction::Stop if context.status.state == State::Play => client.stop()?,
                GlobalAction::ToggleRepeat => client.repeat(!context.status.repeat)?,
                GlobalAction::ToggleRandom => client.random(!context.status.random)?,
                GlobalAction::ToggleSingle if client.version() < Version::new(0, 21, 0) => {
                    client.single(context.status.single.cycle_pre_mpd_24())?;
                }
                GlobalAction::ToggleSingle => client.single(context.status.single.cycle())?,
                GlobalAction::ToggleConsume if client.version() < Version::new(0, 24, 0) => {
                    client.consume(context.status.consume.cycle_pre_mpd_24())?;
                }
                GlobalAction::ToggleConsume => {
                    client.consume(context.status.consume.cycle())?;
                }
                GlobalAction::TogglePause if matches!(context.status.state, State::Play | State::Pause) => {
                    client.pause_toggle()?;
                }
                GlobalAction::TogglePause => {}
                GlobalAction::VolumeUp => {
                    client.set_volume(*context.status.volume.inc_by(context.config.volume_step))?;
                }
                GlobalAction::VolumeDown => {
                    client.set_volume(*context.status.volume.dec_by(context.config.volume_step))?;
                }
                GlobalAction::SeekForward if matches!(context.status.state, State::Play | State::Pause) => {
                    client.seek_current(ValueChange::Increase(5))?;
                }
                GlobalAction::SeekBack if matches!(context.status.state, State::Play | State::Pause) => {
                    client.seek_current(ValueChange::Decrease(5))?;
                }
                GlobalAction::NextTab => {
                    self.change_tab(context.config.next_screen(self.active_tab), client, context)?;
                    context.render()?;
                }
                GlobalAction::PreviousTab => {
                    self.change_tab(context.config.prev_screen(self.active_tab), client, context)?;
                    context.render()?;
                }
                GlobalAction::SwitchToTab(name) => {
                    if context.config.tabs.names.contains(&name) {
                        self.change_tab(name, client, context)?;
                        context.render()?;
                    } else {
                        status_error!("Tab with name '{}' does not exist. Check your configuration.", name);
                    }
                }
                GlobalAction::NextTrack => {}
                GlobalAction::PreviousTrack => {}
                GlobalAction::Stop => {}
                GlobalAction::SeekBack => {}
                GlobalAction::SeekForward => {}
                GlobalAction::ExternalCommand { command, .. } => {
                    run_external(command, create_env(context, std::iter::empty::<&str>(), client)?);
                }
                GlobalAction::Quit => return Ok(KeyHandleResult::Quit),
                GlobalAction::ShowHelp => {
                    let modal = KeybindsModal::new(context);
                    modal!(context, modal);
                }
                GlobalAction::ShowOutputs => {
                    modal!(context, OutputsModal::new(client.outputs()?.0));
                }
                GlobalAction::ShowDecoders => {
                    modal!(context, DecodersModal::new(client.decoders()?.0));
                }
                GlobalAction::ShowCurrentSongInfo => {
                    if let Some(current_song) = context.get_current_song(client)? {
                        modal!(context, SongInfoModal::new(current_song));
                    } else {
                        status_info!("No song is currently playing");
                    }
                }
            }
        };

        Ok(KeyHandleResult::None)
    }

    pub fn before_show(&mut self, context: &mut AppContext, client: &mut impl MpdClient) -> Result<()> {
        self.current_song = try_ret!(client.get_current_song(), "Failed to get current song");
        screen_call!(self, before_show(client, &context))
    }

    pub fn display_message(&mut self, message: String, level: Level) {
        self.status_message = Some(StatusMessage {
            message,
            level,
            created: std::time::Instant::now(),
        });
    }

    pub fn on_ui_app_event(
        &mut self,
        event: UiAppEvent,
        context: &mut AppContext,
        client: &mut impl MpdClient,
    ) -> Result<()> {
        match event {
            UiAppEvent::Modal(modal) => {
                self.modals.push(modal.0);
                self.on_event(UiEvent::ModalOpened, context, client)?;
                context.render()?;
            }
            UiAppEvent::PopModal => {
                self.modals.pop();
                self.on_event(UiEvent::ModalClosed, context, client)?;
                context.render()?;
            }
        }
        Ok(())
    }

    pub fn on_event(
        &mut self,
        mut event: UiEvent,
        context: &mut AppContext,
        client: &mut impl MpdClient,
    ) -> Result<()> {
        match event {
            UiEvent::Player => {
                self.current_song = try_ret!(client.get_current_song(), "Failed get current song");
            }
            UiEvent::Database => {
                status_warn!("The music database has been updated. Some parts of the UI may have been reinitialized to prevent inconsistent behaviours.");
            }
            UiEvent::StoredPlaylist => {}
            UiEvent::LogAdded(_) => {
                #[cfg(debug_assertions)]
                if self
                    .tabs
                    .get(&self.active_tab)
                    .is_some_and(|tab| tab.panes.panes_iter().any(|pane| matches!(pane.pane, PaneType::Logs)))
                {
                    context.render()?;
                }
            }
            UiEvent::Resized { .. } => {}
            UiEvent::ModalOpened => {}
            UiEvent::ModalClosed => {}
            UiEvent::Exit => {}
        }

        for name in context.config.tabs.active_panes {
            match self.panes.get_mut(*name) {
                #[cfg(debug_assertions)]
                Panes::Logs(p) => p.on_event(&mut event, client, context),
                Panes::Queue(p) => p.on_event(&mut event, client, context),
                Panes::Directories(p) => p.on_event(&mut event, client, context),
                Panes::Albums(p) => p.on_event(&mut event, client, context),
                Panes::Artists(p) => p.on_event(&mut event, client, context),
                Panes::Playlists(p) => p.on_event(&mut event, client, context),
                Panes::Search(p) => p.on_event(&mut event, client, context),
                Panes::AlbumArtists(p) => p.on_event(&mut event, client, context),
                Panes::AlbumArt(p) => p.on_event(&mut event, client, context),
            }?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct ModalWrapper(Box<dyn Modal + Send + Sync>);

#[derive(Debug)]
pub enum UiAppEvent {
    Modal(ModalWrapper),
    PopModal,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum UiEvent {
    Player,
    Database,
    StoredPlaylist,
    LogAdded(Vec<u8>),
    Resized { columns: u16, rows: u16 },
    ModalOpened,
    ModalClosed,
    Exit,
}

impl TryFrom<IdleEvent> for UiEvent {
    type Error = ();

    fn try_from(event: IdleEvent) -> Result<Self, ()> {
        Ok(match event {
            IdleEvent::Player => UiEvent::Player,
            IdleEvent::Database => UiEvent::Database,
            IdleEvent::StoredPlaylist => UiEvent::StoredPlaylist,
            _ => return Err(()),
        })
    }
}

pub fn restore_terminal<B: Backend + std::io::Write>(terminal: &mut Terminal<B>, enable_mouse: bool) -> Result<()> {
    if enable_mouse {
        execute!(std::io::stdout(), DisableMouseCapture)?;
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(terminal.show_cursor()?)
}

pub fn setup_terminal(enable_mouse: bool) -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    if enable_mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    Ok(terminal)
}

pub enum KeyHandleResult {
    /// Action does NOT warrant a render
    None,
    /// Exit the application
    Quit,
}

impl From<&Level> for Color {
    fn from(value: &Level) -> Self {
        match value {
            Level::Info => Color::Blue,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
            Level::Debug => Color::LightGreen,
            Level::Trace => Color::Magenta,
        }
    }
}

impl From<&FilterKind> for &'static str {
    fn from(value: &FilterKind) -> Self {
        match value {
            FilterKind::Exact => "Exact match",
            FilterKind::Contains => "Contains value",
            FilterKind::StartsWith => "Starts with value",
            FilterKind::Regex => "Regex",
        }
    }
}

impl std::fmt::Display for FilterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterKind::Exact => write!(f, "Exact match"),
            FilterKind::Contains => write!(f, "Contains value"),
            FilterKind::StartsWith => write!(f, "Starts with value"),
            FilterKind::Regex => write!(f, "Regex"),
        }
    }
}

impl FilterKind {
    fn cycle(&mut self) -> &mut Self {
        *self = match self {
            FilterKind::Exact => FilterKind::Contains,
            FilterKind::Contains => FilterKind::StartsWith,
            FilterKind::StartsWith => FilterKind::Regex,
            FilterKind::Regex => FilterKind::Exact,
        };
        self
    }
}

impl Config {
    fn next_screen(&self, current_screen: TabName) -> TabName {
        let names = self.tabs.names;
        *names
            .iter()
            .enumerate()
            .find(|(_, s)| **s == current_screen)
            .and_then(|(idx, _)| names.get((idx + 1) % names.len()))
            .unwrap_or(&current_screen)
    }

    fn prev_screen(&self, current_screen: TabName) -> TabName {
        let names = self.tabs.names;
        *names
            .iter()
            .enumerate()
            .find(|(_, s)| **s == current_screen)
            .and_then(|(idx, _)| names.get((if idx == 0 { names.len() - 1 } else { idx - 1 }) % names.len()))
            .unwrap_or(&current_screen)
    }

    fn as_header_table_block(&self) -> ratatui::widgets::Block {
        if !self.theme.draw_borders {
            return ratatui::widgets::Block::default();
        }
        Block::default().border_style(self.as_border_style())
    }

    fn as_tabs_block(&self) -> ratatui::widgets::Block {
        if !self.theme.draw_borders {
            return ratatui::widgets::Block::default()/* .padding(Padding::new(0, 0, 1, 1)) */;
        }

        ratatui::widgets::Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_set(border::ONE_EIGHTH_WIDE)
            .border_style(self.as_border_style())
    }

    fn as_border_style(&self) -> ratatui::style::Style {
        self.theme.borders_style
    }

    fn as_focused_border_style(&self) -> ratatui::style::Style {
        self.theme.highlight_border_style
    }

    fn as_text_style(&self) -> ratatui::style::Style {
        self.theme
            .text_color
            .map(|color| Style::default().fg(color))
            .unwrap_or_default()
    }

    fn as_styled_progress_bar(&self) -> widgets::progress_bar::ProgressBar {
        let progress_bar_colors = &self.theme.progress_bar;
        widgets::progress_bar::ProgressBar::default()
            .thumb_style(progress_bar_colors.thumb_style)
            .track_style(progress_bar_colors.track_style)
            .elapsed_style(progress_bar_colors.elapsed_style)
            .elapsed_char(self.theme.progress_bar.symbols[0])
            .thumb_char(self.theme.progress_bar.symbols[1])
            .track_char(self.theme.progress_bar.symbols[2])
    }

    fn as_styled_scrollbar(&self) -> ratatui::widgets::Scrollbar {
        let symbols = self.theme.scrollbar.symbols;
        let track = if symbols[0].is_empty() { None } else { Some(symbols[0]) };
        let begin = if symbols[2].is_empty() { None } else { Some(symbols[2]) };
        let end = if symbols[3].is_empty() { None } else { Some(symbols[3]) };
        ratatui::widgets::Scrollbar::default()
            .orientation(ratatui::widgets::ScrollbarOrientation::VerticalRight)
            .track_symbol(track)
            .thumb_symbol(self.theme.scrollbar.symbols[1])
            .begin_symbol(begin)
            .end_symbol(end)
            .track_style(self.theme.scrollbar.track_style)
            .begin_style(self.theme.scrollbar.ends_style)
            .end_style(self.theme.scrollbar.ends_style)
            .thumb_style(self.theme.scrollbar.thumb_style)
    }
}

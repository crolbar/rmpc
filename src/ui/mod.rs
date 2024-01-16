use std::{io::Stdout, ops::AddAssign, time::Duration};

use ansi_to_tui::IntoText;
use anyhow::{Context, Result};
use crossterm::{
    event::KeyEvent,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::{Alignment, Backend, Constraint, CrosstermBackend, Direction, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};
use strum::{Display, IntoEnumIterator, VariantNames};
use tracing::instrument;

use crate::{
    config::Config,
    mpd::client::Client,
    mpd::{
        commands::{volume::Bound, State as MpdState},
        mpd_client::MpdClient,
    },
    ui::widgets::tabs::Tabs,
};
use crate::{
    mpd::version::Version,
    state::{State, StatusExt},
};

#[cfg(debug_assertions)]
use self::screens::logs::LogsScreen;
use self::{
    modals::{Modal, Modals},
    screens::{
        albums::AlbumsScreen, artists::ArtistsScreen, directories::DirectoriesScreen, playlists::PlaylistsScreen,
        queue::QueueScreen, Screen,
    },
    widgets::progress_bar::ProgressBar,
};

pub mod modals;
pub mod screens;
pub mod utils;
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

impl StatusMessage {
    pub fn new(message: String, level: Level) -> Self {
        Self {
            message,
            level,
            created: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Default)]
pub struct SharedUiState {
    pub status_message: Option<StatusMessage>,
    pub frame_counter: u32,
}

#[derive(Debug)]
pub struct Ui<'a> {
    client: Client<'a>,
    screens: Screens,
    shared_state: SharedUiState,
    active_modal: Option<Modals>,
}

impl<'a> Ui<'a> {
    pub fn new(client: Client<'a>, config: &Config) -> Ui<'a> {
        Self {
            client,
            screens: Screens::new(config),
            shared_state: SharedUiState::default(),
            active_modal: None,
        }
    }
}

#[derive(Debug, Default)]
struct Screens {
    queue: QueueScreen,
    #[cfg(debug_assertions)]
    logs: LogsScreen,
    directories: DirectoriesScreen,
    albums: AlbumsScreen,
    artists: ArtistsScreen,
    playlists: PlaylistsScreen,
}

impl Screens {
    fn new(config: &Config) -> Self {
        Self {
            queue: QueueScreen::new(config),
            #[cfg(debug_assertions)]
            logs: LogsScreen::default(),
            directories: DirectoriesScreen::default(),
            albums: AlbumsScreen::default(),
            artists: ArtistsScreen::default(),
            playlists: PlaylistsScreen::default(),
        }
    }
}

macro_rules! invoke {
    ($screen:expr, $fn:ident, $($param:expr),+) => {
        $screen.$fn($($param),+)
    };
}

macro_rules! screen_call {
    ($self:ident, $app:ident, $fn:ident($($param:expr),+)) => {
        match $app.active_tab {
            screens::Screens::Queue => invoke!($self.screens.queue, $fn, $($param),+),
            #[cfg(debug_assertions)]
            screens::Screens::Logs => invoke!($self.screens.logs, $fn, $($param),+),
            screens::Screens::Directories => invoke!($self.screens.directories, $fn, $($param),+),
            screens::Screens::Artists => invoke!($self.screens.artists, $fn, $($param),+),
            screens::Screens::Albums => invoke!($self.screens.albums, $fn, $($param),+),
            screens::Screens::Playlists => invoke!($self.screens.playlists, $fn, $($param),+),
        }
    }
}

impl Ui<'_> {
    #[instrument(skip_all)]
    pub fn render(&mut self, frame: &mut Frame, app: &mut crate::state::State) -> Result<()> {
        if let Some(bg_color) = app.config.ui.background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), frame.size());
        }
        self.shared_state.frame_counter.add_assign(1);
        if self
            .shared_state
            .status_message
            .as_ref()
            .is_some_and(|m| m.created.elapsed() > std::time::Duration::from_secs(5))
        {
            self.shared_state.status_message = None;
        }
        let [title_area, tabs_area, content_area, bar_area] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Percentage(100),
                    Constraint::Min(1),
                ]
                .as_ref(),
            )
            .split(frame.size())
        else {
            return Ok(());
        };

        let [title_left_area, title_ceter_area, title_right_area] = *Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                [
                    Constraint::Percentage(20),
                    Constraint::Percentage(60),
                    Constraint::Percentage(20),
                ]
                .as_ref(),
            )
            .split(title_area)
        else {
            return Ok(());
        };

        let [song_name_area, song_info_area] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(title_ceter_area.height / 2),
                    Constraint::Length(title_ceter_area.height / 2),
                ]
                .as_ref(),
            )
            .split(title_ceter_area)
        else {
            return Ok(());
        };

        let [volume_area, states_area] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(title_ceter_area.height / 2),
                    Constraint::Length(title_ceter_area.height / 2),
                ]
                .as_ref(),
            )
            .split(title_right_area)
        else {
            return Ok(());
        };

        let [status_area, elapsed_area] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(title_ceter_area.height / 2),
                    Constraint::Length(title_ceter_area.height / 2),
                ]
                .as_ref(),
            )
            .split(title_left_area)
        else {
            return Ok(());
        };

        let tab_names = screens::Screens::VARIANTS
            .iter()
            .map(|e| format!("{: ^13}", format!("{e}")))
            .collect::<Vec<String>>();

        let tabs = Tabs::new(tab_names)
            .select(
                screens::Screens::iter()
                    .enumerate()
                    .find(|(_, t)| t == &app.active_tab)
                    .context("No active tab found. This really should not happen since we iterate over all the enum values as provided by strum.")?
                    .0,
            )
            .divider("")
            .block(ratatui::widgets::Block::default().borders(Borders::TOP).border_style(Style::default().fg(app.config.ui.borders_color)))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Blue));

        // right
        let volume = crate::ui::widgets::volume::Volume::default()
            .value(*app.status.volume.value())
            .alignment(Alignment::Right)
            .style(Style::default().fg(app.config.ui.volume_color));

        let on_style = Style::default().fg(Color::Gray);
        let off_style = Style::default().fg(Color::DarkGray);
        let separator = Span::styled(" / ", on_style);
        let states = Paragraph::new(Line::from(vec![
            Span::styled("Repeat", if app.status.repeat { on_style } else { off_style }),
            separator.clone(),
            Span::styled("Random", if app.status.random { on_style } else { off_style }),
            separator.clone(),
            match app.status.consume {
                crate::mpd::commands::status::OnOffOneshot::On => Span::styled("Consume", on_style),
                crate::mpd::commands::status::OnOffOneshot::Off => Span::styled("Consume", off_style),
                crate::mpd::commands::status::OnOffOneshot::Oneshot => Span::styled("Oneshot(C)", on_style),
            },
            separator,
            match app.status.single {
                crate::mpd::commands::status::OnOffOneshot::On => Span::styled("Single", on_style),
                crate::mpd::commands::status::OnOffOneshot::Off => Span::styled("Single", off_style),
                crate::mpd::commands::status::OnOffOneshot::Oneshot => Span::styled("Oneshot(S)", on_style),
            },
        ]))
        .alignment(Alignment::Right);

        // center
        let song_name = Paragraph::new(
            app.current_song
                .as_ref()
                .map_or("No song", |v| v.title.as_ref().map_or("No song", |v| v.as_str())),
        )
        .style(Style::default().bold())
        .alignment(Alignment::Center);

        // left
        // no rendered frames in release mode
        #[cfg(debug_assertions)]
        let status = Paragraph::new(Span::styled(
            format!(
                "[{}] {} rendered frames",
                app.status.state, self.shared_state.frame_counter
            ),
            Style::default().fg(app.config.ui.status_color),
        ));
        #[cfg(not(debug_assertions))]
        let status = Paragraph::new(Span::styled(
            format!("[{}]", app.status.state),
            Style::default().fg(app.config.ui.status_color),
        ));

        let elapsed = if app.config.status_update_interval_ms.is_some() {
            Paragraph::new(format!(
                "{}/{}{}",
                app.status.elapsed.to_string(),
                app.status.duration.to_string(),
                app.status.bitrate()
            ))
        } else {
            Paragraph::new(format!("{}{}", app.status.duration.to_string(), app.status.bitrate()))
        }
        .style(Style::default().fg(Color::Gray));

        let song_info = Paragraph::new(app.current_song.as_ref().map_or(Line::default(), |v| {
            let artist = v.artist.as_ref().map_or("Unknown", |v| v.as_str());
            let album = v.album.as_ref().map_or("Unknown Album", |v| v.as_str());
            Line::from(vec![
                Span::styled(artist, Style::default().fg(Color::Yellow)),
                Span::styled(" - ", Style::default().bold()),
                Span::styled(album, Style::default().fg(Color::LightBlue)),
            ])
        }))
        .alignment(Alignment::Center);

        if let Some(StatusMessage {
            ref message, ref level, ..
        }) = self.shared_state.status_message
        {
            let status_bar = Paragraph::new(
                message
                    .into_text()
                    .context("Failed to convert status bar message to text")?,
            )
            .alignment(ratatui::prelude::Alignment::Center)
            .style(Style::default().fg(level.to_color()).bg(Color::Black));
            frame.render_widget(status_bar, bar_area);
        } else if app.config.status_update_interval_ms.is_some() {
            let elapsed_bar = ProgressBar::default()
                .thumb_style(
                    Style::default().fg(app.config.ui.progress_bar.thumb_colors.0).bg(app
                        .config
                        .ui
                        .progress_bar
                        .thumb_colors
                        .1),
                )
                .track_style(
                    Style::default().fg(app.config.ui.progress_bar.track_colors.0).bg(app
                        .config
                        .ui
                        .progress_bar
                        .track_colors
                        .1),
                )
                .elapsed_style(
                    Style::default().fg(app.config.ui.progress_bar.elapsed_colors.0).bg(app
                        .config
                        .ui
                        .progress_bar
                        .elapsed_colors
                        .1),
                )
                .elapsed_char(app.config.ui.progress_bar.symbols[0])
                .thumb_char(app.config.ui.progress_bar.symbols[1])
                .track_char(app.config.ui.progress_bar.symbols[2]);
            let elapsed_bar = if app.status.duration == Duration::ZERO {
                elapsed_bar.value(0.0)
            } else {
                elapsed_bar.value(app.status.elapsed.as_secs_f32() / app.status.duration.as_secs_f32())
            };
            frame.render_widget(elapsed_bar, bar_area);
        }

        frame.render_widget(states, states_area);
        frame.render_widget(status, status_area);
        frame.render_widget(elapsed, elapsed_area);
        frame.render_widget(volume, volume_area);
        frame.render_widget(song_name, song_name_area);
        frame.render_widget(song_info, song_info_area);
        frame.render_widget(tabs, tabs_area);

        screen_call!(self, app, render(frame, content_area, app, &mut self.shared_state))?;

        if let Some(ref mut modal) = self.active_modal {
            Self::render_modal(modal, frame, app, &mut self.shared_state)?;
        }

        Ok(())
    }

    fn render_modal(
        active_modal: &mut modals::Modals,
        frame: &mut Frame<'_>,
        app: &mut State,
        shared: &mut SharedUiState,
    ) -> Result<()> {
        match active_modal {
            modals::Modals::ConfirmQueueClear(ref mut m) => m.render(frame, app, shared),
            modals::Modals::SaveQueue(ref mut m) => m.render(frame, app, shared),
            modals::Modals::RenamePlaylist(ref mut m) => m.render(frame, app, shared),
            modals::Modals::AddToPlaylist(ref mut m) => m.render(frame, app, shared),
        }
    }
    fn handle_modal_key(
        active_modal: &mut modals::Modals,
        client: &mut Client<'_>,
        key: KeyEvent,
        app: &mut State,
        shared: &mut SharedUiState,
    ) -> Result<KeyHandleResultInternal> {
        match active_modal {
            modals::Modals::ConfirmQueueClear(ref mut m) => m.handle_key(key, client, app, shared),
            modals::Modals::SaveQueue(ref mut m) => m.handle_key(key, client, app, shared),
            modals::Modals::RenamePlaylist(ref mut m) => m.handle_key(key, client, app, shared),
            modals::Modals::AddToPlaylist(ref mut m) => m.handle_key(key, client, app, shared),
        }
    }

    #[instrument(skip(self, app), fields(screen))]
    pub fn handle_key(&mut self, key: KeyEvent, app: &mut State) -> Result<KeyHandleResult> {
        macro_rules! screen_call_inner {
            ($fn:ident($($param:expr),+)) => {
                screen_call!(self, app, $fn($($param),+))?
            }
        }
        if let Some(ref mut modal) = self.active_modal {
            return match Self::handle_modal_key(modal, &mut self.client, key, app, &mut self.shared_state)? {
                KeyHandleResultInternal::Modal(None) => {
                    self.active_modal = None;
                    screen_call_inner!(refresh(&mut self.client, app, &mut self.shared_state));
                    Ok(KeyHandleResult::RenderRequested)
                }
                r => Ok(r.into()),
            };
        }

        match screen_call_inner!(handle_action(key, &mut self.client, app, &mut self.shared_state)) {
            KeyHandleResultInternal::RenderRequested => return Ok(KeyHandleResult::RenderRequested),
            KeyHandleResultInternal::SkipRender => return Ok(KeyHandleResult::SkipRender),
            KeyHandleResultInternal::Modal(modal) => {
                self.active_modal = modal;
                return Ok(KeyHandleResult::RenderRequested);
            }
            KeyHandleResultInternal::KeyNotHandled => {
                if let Some(action) = app.config.keybinds.global.get(&key.into()) {
                    match action {
                        GlobalAction::NextTrack if app.status.state == MpdState::Play => self.client.next()?,
                        GlobalAction::PreviousTrack if app.status.state == MpdState::Play => self.client.prev()?,
                        GlobalAction::Stop if app.status.state == MpdState::Play => self.client.stop()?,
                        GlobalAction::ToggleRepeat => self.client.repeat(!app.status.repeat)?,
                        GlobalAction::ToggleSingle => self.client.single(app.status.single.cycle())?,
                        GlobalAction::ToggleRandom => self.client.random(!app.status.random)?,
                        GlobalAction::ToggleConsume if self.client.version < Version::new(0, 24, 0) => {
                            self.client.consume(app.status.consume.cycle_pre_mpd_24())?;
                        }
                        GlobalAction::ToggleConsume => {
                            self.client.consume(app.status.consume.cycle())?;
                        }
                        GlobalAction::TogglePause
                            if app.status.state == MpdState::Play || app.status.state == MpdState::Pause =>
                        {
                            self.client.pause_toggle()?;
                            return Ok(KeyHandleResult::SkipRender);
                        }
                        GlobalAction::TogglePause => {}
                        GlobalAction::VolumeUp => {
                            self.client
                                .set_volume(app.status.volume.inc_by(app.config.volume_step))?;
                        }
                        GlobalAction::VolumeDown => {
                            self.client
                                .set_volume(app.status.volume.dec_by(app.config.volume_step))?;
                        }
                        GlobalAction::SeekForward if app.status.state == MpdState::Play => {
                            self.client.seek_curr_forwards(5)?;
                        }
                        GlobalAction::SeekBack if app.status.state == MpdState::Play => {
                            self.client.seek_curr_backwards(5)?;
                        }
                        GlobalAction::NextTab => {
                            screen_call_inner!(on_hide(&mut self.client, app, &mut self.shared_state));

                            app.active_tab = app.active_tab.next();
                            tracing::Span::current().record("screen", app.active_tab.to_string());
                            screen_call_inner!(before_show(&mut self.client, app, &mut self.shared_state));
                            return Ok(KeyHandleResult::RenderRequested);
                        }
                        GlobalAction::PreviousTab => {
                            screen_call_inner!(on_hide(&mut self.client, app, &mut self.shared_state));

                            app.active_tab = app.active_tab.prev();
                            tracing::Span::current().record("screen", app.active_tab.to_string());
                            screen_call_inner!(before_show(&mut self.client, app, &mut self.shared_state));
                            return Ok(KeyHandleResult::RenderRequested);
                        }
                        GlobalAction::NextTrack => {}
                        GlobalAction::PreviousTrack => {}
                        GlobalAction::Stop => {}
                        GlobalAction::SeekBack => {}
                        GlobalAction::SeekForward => {}
                        GlobalAction::Quit => return Ok(KeyHandleResult::Quit),
                    }
                    Ok(KeyHandleResult::SkipRender)
                } else {
                    Ok(KeyHandleResult::SkipRender)
                }
            }
        }
    }

    #[instrument(skip_all)]
    pub fn before_show(&mut self, app: &mut State) -> Result<()> {
        screen_call!(self, app, before_show(&mut self.client, app, &mut self.shared_state))
    }

    pub fn display_message(&mut self, message: String, level: Level) {
        self.shared_state.status_message = Some(StatusMessage {
            message,
            level,
            created: std::time::Instant::now(),
        });
    }
}

#[derive(Debug, Display, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash, Clone, Copy)]
pub enum GlobalAction {
    Quit,
    NextTrack,
    PreviousTrack,
    Stop,
    ToggleRepeat,
    ToggleSingle,
    ToggleRandom,
    ToggleConsume,
    TogglePause,
    VolumeUp,
    VolumeDown,
    SeekForward,
    SeekBack,
    NextTab,
    PreviousTab,
}

pub fn restore_terminal<B: Backend + std::io::Write>(terminal: &mut Terminal<B>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(terminal.show_cursor()?)
}

pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    Ok(terminal)
}

enum KeyHandleResultInternal {
    /// Action warrants a render
    RenderRequested,
    /// Action does NOT warrant a render
    SkipRender,
    /// Event was not handled and should bubble up
    KeyNotHandled,
    /// Display a modal
    Modal(Option<Modals>),
}

pub enum KeyHandleResult {
    /// Action warrants a render
    RenderRequested,
    /// Action does NOT warrant a render
    SkipRender,
    /// Exit the application
    Quit,
}

impl From<KeyHandleResultInternal> for KeyHandleResult {
    fn from(value: KeyHandleResultInternal) -> Self {
        match value {
            KeyHandleResultInternal::SkipRender => KeyHandleResult::SkipRender,
            _ => KeyHandleResult::RenderRequested,
        }
    }
}

trait LevelExt {
    fn to_color(&self) -> Color;
}
impl LevelExt for Level {
    fn to_color(&self) -> Color {
        match *self {
            Level::Info => Color::Blue,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
            Level::Debug => Color::LightGreen,
            Level::Trace => Color::Magenta,
        }
    }
}

trait DurationExt {
    fn to_string(&self) -> String;
}

impl DurationExt for Duration {
    fn to_string(&self) -> String {
        let secs = self.as_secs();
        let min = secs / 60;
        format!("{}:{:0>2}", min, secs - min * 60)
    }
}

trait BoolExt {
    fn to_onoff(&self) -> &'static str;
}

impl BoolExt for bool {
    fn to_onoff(&self) -> &'static str {
        if *self {
            "On"
        } else {
            "Off"
        }
    }
}

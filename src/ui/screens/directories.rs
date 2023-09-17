use std::{cmp::Ordering, path::PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    prelude::{Backend, Rect},
    widgets::ListItem,
    Frame,
};
use tracing::instrument;

use crate::{
    mpd::{
        client::Client,
        commands::{lsinfo::FileOrDir, Song},
        mpd_client::MpdClient,
    },
    state::State,
    ui::{widgets::browser::Browser, KeyHandleResult, Level, SharedUiState, StatusMessage},
};

use super::{
    browser::{DirOrSong, DirOrSongInfo, ToListItems},
    dirstack::DirStack,
    iter::DirOrSongListItems,
    CommonAction, Screen, SongExt,
};

#[derive(Debug)]
pub struct DirectoriesScreen {
    stack: DirStack<DirOrSongInfo>,
    filter_input_mode: bool,
    path: PathBuf,
}

impl Default for DirectoriesScreen {
    fn default() -> Self {
        Self {
            stack: DirStack::new(Vec::new()),
            filter_input_mode: false,
            path: PathBuf::new(),
        }
    }
}

#[async_trait]
impl Screen for DirectoriesScreen {
    type Actions = DirectoriesActions;
    fn render<B: Backend>(
        &mut self,
        frame: &mut Frame<B>,
        area: Rect,
        app: &mut crate::state::State,
        _state: &mut SharedUiState,
    ) -> anyhow::Result<()> {
        let w = Browser::new(&app.config.symbols, &app.config.column_widths);
        frame.render_stateful_widget(w, area, &mut self.stack);

        Ok(())
    }

    async fn before_show(
        &mut self,
        client: &mut Client<'_>,
        _app: &mut crate::state::State,
        _shared: &mut SharedUiState,
    ) -> Result<()> {
        self.path = PathBuf::new();
        self.stack = DirStack::new(client.lsinfo(None).await?.0.into_iter().map(Into::into).collect());
        self.stack.preview = self
            .prepare_preview(client, _app)
            .await
            .context("Cannot prepare preview")?;

        Ok(())
    }

    #[instrument(skip_all)]
    async fn handle_action(
        &mut self,
        event: KeyEvent,
        client: &mut Client<'_>,
        app: &mut State,
        shared: &mut SharedUiState,
    ) -> Result<KeyHandleResult> {
        if self.filter_input_mode {
            match event.code {
                KeyCode::Char(c) => {
                    if let Some(ref mut f) = self.stack.filter {
                        f.push(c);
                    };
                    Ok(KeyHandleResult::RenderRequested)
                }
                KeyCode::Backspace => {
                    if let Some(ref mut f) = self.stack.filter {
                        f.pop();
                    };
                    Ok(KeyHandleResult::RenderRequested)
                }
                KeyCode::Enter => {
                    self.filter_input_mode = false;
                    self.stack.jump_forward();
                    Ok(KeyHandleResult::RenderRequested)
                }
                KeyCode::Esc => {
                    self.filter_input_mode = false;
                    self.stack.filter = None;
                    Ok(KeyHandleResult::RenderRequested)
                }
                _ => Ok(KeyHandleResult::SkipRender),
            }
        } else if let Some(action) = app.config.keybinds.directories.get(&event.into()) {
            match action {
                DirectoriesActions::AddAll => {
                    match self.stack.get_selected() {
                        Some(DirOrSongInfo::Dir(dir)) => {
                            let mut path_to_add = self.path.clone();
                            path_to_add.push(dir);

                            if let Some(path) = path_to_add.to_str() {
                                client.add(path).await?;
                                shared.status_message = Some(StatusMessage::new(
                                    format!("Directory '{path}' added to queue"),
                                    Level::Info,
                                ));
                            } else {
                                tracing::error!(message = "Failed to add directory to queue.", dir = ?path_to_add);
                                shared.status_message = Some(StatusMessage::new(
                                    format!("Failed to add directory '{path_to_add:?}' to queue."),
                                    Level::Error,
                                ));
                            }
                        }
                        Some(DirOrSongInfo::Song(song)) => {
                            client.add(&song.file).await?;
                            shared.status_message = Some(StatusMessage::new(
                                format!("'{}' by '{}' added to queue", song.title_str(), song.artist_str(),),
                                Level::Info,
                            ));
                        }
                        None => {}
                    };
                    Ok(KeyHandleResult::RenderRequested)
                }
            }
        } else if let Some(action) = app.config.keybinds.navigation.get(&event.into()) {
            match action {
                CommonAction::DownHalf => {
                    self.stack.next_half_viewport();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::UpHalf => {
                    self.stack.prev_half_viewport();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::Up => {
                    self.stack.prev();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::Down => {
                    self.stack.next();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::Bottom => {
                    self.stack.last();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::Top => {
                    self.stack.first();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::Right => {
                    match self.stack.get_selected().context("Expected an item to be selected")? {
                        DirOrSongInfo::Dir(dir) => {
                            self.path.push(dir);
                            let new_current = client.lsinfo(self.path.to_str()).await?.0;
                            self.stack.push(
                                new_current
                                    .into_iter()
                                    .map(|v| match v {
                                        FileOrDir::Dir(d) => DirOrSongInfo::Dir(d.path),
                                        FileOrDir::File(s) => DirOrSongInfo::Song(s),
                                    })
                                    .collect(),
                            );

                            self.stack.preview = self
                                .prepare_preview(client, app)
                                .await
                                .context("Cannot prepare preview")?;
                        }
                        DirOrSongInfo::Song(song) => {
                            client.add(&song.file).await?;
                            shared.status_message = Some(StatusMessage::new(
                                format!("'{}' by '{}' added to queue", song.title_str(), song.artist_str(),),
                                Level::Info,
                            ));
                        }
                    };
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::Left => {
                    self.stack.pop();
                    self.path.pop();
                    self.stack.preview = self
                        .prepare_preview(client, app)
                        .await
                        .context("Cannot prepare preview")?;
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::EnterSearch => {
                    self.filter_input_mode = true;
                    self.stack.filter = Some(String::new());
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::NextResult => {
                    self.stack.jump_forward();
                    Ok(KeyHandleResult::RenderRequested)
                }
                CommonAction::PreviousResult => {
                    self.stack.jump_back();
                    Ok(KeyHandleResult::RenderRequested)
                }
            }
        } else {
            Ok(KeyHandleResult::KeyNotHandled)
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum DirectoriesActions {
    AddAll,
}

impl DirectoriesScreen {
    #[instrument(skip(client))]
    async fn prepare_preview(&mut self, client: &mut Client<'_>, state: &State) -> Option<Vec<ListItem<'static>>> {
        let idx = self.stack.current().1.get_selected()?;
        match &self.stack.current().0[idx] {
            DirOrSongInfo::Dir(dir) => {
                let mut preview_path = self.path.clone();
                preview_path.push(dir);
                let mut res = match client.lsinfo(preview_path.to_str()).await {
                    Ok(val) => val,
                    Err(err) => {
                        tracing::error!(message = "Failed to get lsinfo for dir", error = ?err);
                        return None;
                    }
                }
                .0;
                res.sort();
                Some(
                    res.into_iter()
                        .map(|v| match v {
                            FileOrDir::Dir(dir) => DirOrSong::Dir(dir.path),
                            FileOrDir::File(song) => {
                                DirOrSong::Song(song.title.as_ref().map_or("Untitled", |v| v.as_str()).to_owned())
                            }
                        })
                        .listitems(&state.config.symbols)
                        .collect(),
                )
            }
            DirOrSongInfo::Song(song) => Some(song.to_listitems(&state.config.symbols)),
        }
    }
}

impl std::cmp::Ord for FileOrDir {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (_, FileOrDir::Dir(_)) => Ordering::Greater,
            (FileOrDir::Dir(_), _) => Ordering::Less,
            (FileOrDir::File(Song { title: t1, .. }), FileOrDir::File(Song { title: t2, .. })) => t1.cmp(t2),
        }
    }
}
impl std::cmp::PartialOrd for FileOrDir {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (_, FileOrDir::Dir(_)) => Some(Ordering::Greater),
            (FileOrDir::Dir(_), _) => Some(Ordering::Less),
            (FileOrDir::File(Song { title: t1, .. }), FileOrDir::File(Song { title: t2, .. })) => Some(t1.cmp(t2)),
        }
    }
}

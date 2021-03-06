use std::borrow::BorrowMut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arraydeque::{ArrayDeque, CapacityError};
use futures::Future;
use lavalink_rs::error::LavalinkError;
use lavalink_rs::model::{PlayerUpdate, Track, TrackFinish, TrackStart};
use lavalink_rs::LavalinkClient;
use serenity::model::prelude::{ChannelId, GuildId, UserId};
use songbird::error::JoinError;
use songbird::Songbird;
use thiserror::Error;
use tokio::sync::watch::{Receiver as WatchReceiver, Sender as WatchSender};

use std::fmt::{Display, Formatter};
use strum_macros::AsRefStr;

const MUSIC_QUEUE_LIMIT: usize = 100;

pub struct Player {
    channel: ChannelId,
    guild: GuildId,
    lavalink: LavalinkClient,
    songbird: Arc<Songbird>,
    player_state: PlayerState,

    send: WatchSender<Arc<PlayerState>>,
    receive: WatchReceiver<Arc<PlayerState>>,
}

impl Player {
    pub async fn new(
        bot: UserId,
        channel: ChannelId,
        guild: GuildId,
        songbird: Arc<Songbird>,
        lavalink: LavalinkClient,
    ) -> Result<(Player, WatchReceiver<Arc<PlayerState>>), PlayerError> {
        let connection_info = songbird
            .join_gateway(guild, channel)
            .await
            .1
            .map_err(PlayerError::SongbirdJoin)?;
        lavalink
            .create_session(&connection_info)
            .await
            .map_err(PlayerError::Lavalink)?;

        let player_state = PlayerState::new(bot);
        let (send, receive) = tokio::sync::watch::channel(Arc::new(player_state.clone()));

        let player = Player {
            channel,
            guild,
            lavalink,
            songbird,
            player_state,

            send,
            receive: receive.clone(),
        };

        Ok((player, receive))
    }

    pub fn get_status_watch(&self) -> WatchReceiver<Arc<PlayerState>> {
        self.receive.clone()
    }

    pub fn get_lavalink(&self) -> LavalinkClient {
        self.lavalink.clone()
    }

    pub fn get_channel(&self) -> ChannelId {
        self.channel
    }

    pub fn get_bot(&self) -> UserId {
        self.player_state.bot
    }

    pub async fn resume(&mut self) -> Result<(), PlayerError> {
        self.lavalink
            .resume(self.guild)
            .await
            .map_err(PlayerError::Lavalink)?;
        self.player_state.play_state = PlayState::Play;

        self.send.send(Arc::new(self.player_state.clone())).ok();
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<(), PlayerError> {
        self.lavalink
            .pause(self.guild)
            .await
            .map_err(PlayerError::Lavalink)?;
        self.player_state.play_state = PlayState::Pause;

        self.send.send(Arc::new(self.player_state.clone())).ok();
        Ok(())
    }

    pub async fn dynamic_pause_resume(&mut self) -> Result<(), PlayerError> {
        match self.player_state.play_state {
            PlayState::Play => self.pause().await,
            PlayState::Pause => self.resume().await,
        }
    }

    pub async fn skip(&mut self, i: usize) -> Result<(), PlayerError> {
        //Leave if skip amount is 0
        if i == 0 {
            return Ok(());
        }

        let mut changed = false;

        //If loop is one, move the current track to history, so a new Track gets played
        if let Some((_, track)) = self.player_state.current.take() {
            match self.player_state.playback {
                Playback::AllLoop => self.push_to_playlist_back(track),
                _ => self.push_to_history_front(track),
            }
            changed = true;
        }

        for _i in 0..i - 1 {
            if let Some(track) = self.player_state.playlist.pop_front() {
                match self.player_state.playback {
                    Playback::AllLoop => self.push_to_playlist_back(track),
                    _ => self.push_to_history_front(track),
                }
            } else {
                break;
            }
        }

        if changed {
            self.send.send(Arc::new(self.player_state.clone())).ok();
        }

        self.lavalink
            .stop(self.guild)
            .await
            .map_err(PlayerError::Lavalink)
    }

    pub async fn back_skip(&mut self, i: usize) -> Result<(), PlayerError> {
        if i == 0 {
            return Ok(());
        }

        let mut changed = false;
        let mut current_was_some = false;

        if let Some((_, track)) = self.player_state.current.take() {
            self.push_to_playlist_front(track);
            changed = true;
            current_was_some = true;
        }

        for _i in 0..i {
            if let Some(history_track) = self.player_state.history.pop_front() {
                self.push_to_playlist_front(history_track);
                changed = true;
            }
        }

        if changed {
            self.send.send(Arc::new(self.player_state.clone())).ok();
        }
        if current_was_some {
            self.lavalink
                .stop(self.guild)
                .await
                .map_err(PlayerError::Lavalink)
        } else {
            self.play_next().await
        }
    }

    fn push_to_history_front(&mut self, track: Track) {
        if self.player_state.history.is_full() {
            self.player_state
                .history
                .pop_back()
                .expect("History is empty");
        }
        self.player_state
            .history
            .push_front(track)
            .expect("History is full");
    }

    fn push_to_playlist_back(&mut self, track: Track) {
        if self.player_state.playlist.is_full() {
            self.player_state
                .playlist
                .pop_back()
                .expect("Playlist is empty");
        }
        self.player_state
            .playlist
            .push_back(track)
            .expect("Playlist is full");
    }

    fn push_to_playlist_front(&mut self, track: Track) {
        if self.player_state.playlist.is_full() {
            self.player_state
                .playlist
                .pop_back()
                .expect("Playlist is empty");
        }
        self.player_state
            .playlist
            .push_front(track)
            .expect("Playlist is full");
    }

    pub async fn enqueue(
        &mut self,
        tracks: impl Iterator<Item = Track>,
    ) -> Result<(), PlayerError> {
        for (i, track) in tracks.enumerate() {
            let res = self.player_state.playlist.push_back(track);
            if i == 0 {
                res.map_err(PlayerError::PlaylistFull)?;
            }
        }

        if self.player_state.current.is_none() {
            self.play_next().await?;
        } else {
            self.send.send(Arc::new(self.player_state.clone())).ok();
        }
        Ok(())
    }

    pub async fn jump(&mut self, pos: Duration) -> Result<(), PlayerError> {
        if self.player_state.current.is_some() {
            return self
                .lavalink
                .jump_to_time(self.guild, pos)
                .await
                .map_err(PlayerError::Lavalink);
        }

        return Err(PlayerError::NoCurrentSong());
    }

    pub fn clear_queue(&mut self) {
        if !self.player_state.playlist.is_empty() {
            self.player_state.playlist.clear();
            self.send.send(Arc::new(self.player_state.clone())).ok();
        }
    }

    pub fn playback(&mut self, playback: Playback) {
        if self.player_state.playback != playback {
            self.player_state.playback = playback;
        } else {
            self.player_state.playback = Playback::Normal;
        }
        self.send.send(Arc::new(self.player_state.clone())).ok();
    }

    pub async fn disconnect(self) -> Result<(), PlayerError> {
        self.songbird
            .get(self.guild)
            .ok_or(PlayerError::NotInAVoiceChannel())?;
        self.songbird
            .remove(self.guild)
            .await
            .map_err(PlayerError::SongbirdLeave)?;
        self.lavalink
            .destroy(self.guild)
            .await
            .map_err(PlayerError::Lavalink)?;

        Ok(())
    }

    pub fn search<F, Fut>(&self, query: String, callback: F)
    where
        F: Send + Sync + 'static + FnOnce(Result<Vec<Track>, PlayerError>) -> Fut,
        Fut: Future<Output = ()> + Send + Sync,
    {
        let lavalink = self.lavalink.clone();
        tokio::spawn(async move {
            match lavalink.auto_search_tracks(query).await {
                Err(e) => callback(Err(PlayerError::Lavalink(e))).await,
                Ok(tracks) => {
                    if tracks.load_type.eq("LOAD_FAILED") {
                        callback(Err(PlayerError::SearchFailed(tracks.load_type))).await;
                        return;
                    }
                    callback(Ok(tracks.tracks)).await;
                }
            }
        });
    }

    async fn play_next(&mut self) -> Result<(), PlayerError> {
        let mut changed = false;

        match self.player_state.playback {
            //Add Current to History
            Playback::Normal => {
                if let Some((_, track)) = self.player_state.current.take() {
                    self.push_to_history_front(track);
                    changed = true;
                }
            }
            //Add Current to Playlist
            Playback::AllLoop => {
                if let Some((_, track)) = self.player_state.current.take() {
                    self.push_to_playlist_back(track);
                    changed = true;
                }
            }
            //Reset Duration of Current
            Playback::OneLoop => {
                if let Some(((duration, instant), _)) = self.player_state.current.borrow_mut() {
                    *duration = Duration::from_secs(0);
                    *instant = Instant::now();
                }
            }
        }

        //If current is None: Pull new one from Playlist
        if self.player_state.current.is_none() {
            if let Some(track) = self.player_state.playlist.pop_front() {
                self.player_state.current = Some(((Duration::from_secs(0), Instant::now()), track));
                self.player_state.play_state = PlayState::Play;
                changed = true;
            }
        }

        if changed {
            self.send.send(Arc::new(self.player_state.clone())).ok();
        }

        //Start if Current is some. Stop if Current is none.
        match &self.player_state.current {
            None => self
                .lavalink
                .stop(self.guild)
                .await
                .map_err(PlayerError::Lavalink),
            Some((_, track)) => self
                .lavalink
                .play(self.guild, track.clone())
                .start()
                .await
                .map_err(PlayerError::Lavalink),
        }
    }

    pub fn update(&mut self, update: PlayerUpdate) {
        let now = Instant::now();
        let new_pos = Duration::from_millis(update.state.position as u64);
        if let Some(((pos, when), _)) = self.player_state.current.borrow_mut() {
            *pos = new_pos;
            *when = now;
            self.send.send(Arc::new(self.player_state.clone())).ok();
        }
    }

    pub async fn track_end(&mut self, _end: TrackFinish) -> Result<(), PlayerError> {
        self.play_next().await
    }

    pub fn track_start(&mut self, _start: TrackStart) {
        //TODO maybe do something, maybe dont
    }
}

#[derive(Copy, Clone, Debug, AsRefStr, Eq, PartialEq)]
pub enum PlayState {
    Play,
    Pause,
}

impl Display for PlayState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayState::Play => Ok(()),
            PlayState::Pause => write!(f, "Paused "),
        }
    }
}

impl PlayState {
    pub fn is_paused(&self) -> bool {
        match self {
            PlayState::Play => false,
            PlayState::Pause => true,
        }
    }
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum Playback {
    Normal,
    AllLoop,
    OneLoop,
}

impl Display for Playback {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Playback::Normal => Ok(()),
            Playback::AllLoop => write!(f, "🔁"),
            Playback::OneLoop => write!(f, "🔂"),
        }
    }
}

#[derive(Error, Debug)]
pub enum PlayerError {
    #[error("LavalinkError occurred: {0:?}")]
    Lavalink(LavalinkError),
    #[error("Error joining Channel: {0:?}")]
    SongbirdJoin(JoinError),
    #[error("Error leaving Channel: {0:?}")]
    SongbirdLeave(JoinError),
    #[error("Not in a Voice Channel")]
    NotInAVoiceChannel(),
    #[error("Playlist is full: {0:?}")]
    PlaylistFull(CapacityError<Track>),
    #[error("Search failed: {0:?}")]
    SearchFailed(String),
    #[error("There is no current song")]
    NoCurrentSong(),
}

impl PlayerError {
    pub fn is_lavalink_error(&self) -> bool {
        matches!(self, PlayerError::Lavalink(_))
    }

    pub fn is_fatal(&self) -> bool {
        if let PlayerError::Lavalink(LavalinkError::ErrorWebsocketPayload(
            tokio_tungstenite::tungstenite::Error::ConnectionClosed,
        )) = self
        {
            return true;
        }
        false
    }
}

#[derive(Clone, Debug)]
pub struct PlayerState {
    pub bot: UserId,
    pub current: Option<((Duration, Instant), Track)>,
    pub playlist: ArrayDeque<[Track; MUSIC_QUEUE_LIMIT]>,
    pub history: ArrayDeque<[Track; MUSIC_QUEUE_LIMIT]>,
    pub play_state: PlayState,
    pub playback: Playback,
}

impl PlayerState {
    fn new(bot: UserId) -> Self {
        PlayerState {
            bot,
            current: None,
            playlist: ArrayDeque::new(),
            history: ArrayDeque::new(),
            play_state: PlayState::Play,
            playback: Playback::Normal,
        }
    }
}

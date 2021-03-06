use crate::bots::BotMap;
use crate::config::NetConfig;
use crate::guild::player_manager::{PlayerManager, PlayerRequest};
use crate::player::Player;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use reciprocity_communication::host::*;
use reciprocity_communication::messages::oauth2::AccessToken;
use reciprocity_communication::messages::{
    Auth, AuthMessage, BotInfo, ClientRequest, Message, PlayMode, PlayerControl,
    PlayerControlResult, PlayerState, State, Track, Unexpected, User, VoiceState,
};
use serenity::model::prelude::{ChannelId, GuildId, UserId};
use serenity::model::user::CurrentUser;
use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};

#[derive(Clone)]
pub struct CompanionCommunicationHandler {
    players: Arc<HashMap<GuildId, Arc<PlayerManager>>>,
    bots: Arc<BotMap>,
}

impl CompanionCommunicationHandler {
    pub fn new(
        cfg: NetConfig,
        bots: Arc<BotMap>,
        players: Arc<HashMap<GuildId, Arc<PlayerManager>>>,
    ) -> Self {
        let comp = CompanionCommunicationHandler { players, bots };

        tokio::spawn(comp.clone().run(cfg));
        comp
    }

    async fn run(self, cfg: NetConfig) {
        info!("Starting Net Receiver Loop. {:?}", cfg);
        let addr = SocketAddrV4::new(cfg.address, cfg.port);
        let lis_res = TcpListener::bind(addr).await;
        let listener = match lis_res {
            Ok(l) => l,
            Err(e) => {
                let msg = format!("Error building TCP Listener. {:?}", e);
                error!("{}", msg);
                panic!("{}", msg);
            }
        };
        info!("Listening now: {:?}", addr);

        while let Ok((stream, _)) = listener.accept().await {
            let peer_res = stream.peer_addr();
            let peer = match peer_res {
                Ok(p) => p,
                Err(e) => {
                    warn!("Error getting Peer Address: {:?}", e);
                    continue;
                }
            };
            info!("Connection from Peer: {:?}", peer);

            tokio::spawn(self.clone().handle_connection(peer, stream));
        }

        let msg = format!("Tcp Listener Ended. {:?}", addr);
        error!("{}", msg);
        panic!("{}", msg);
    }

    async fn handle_connection(self, peer: SocketAddr, stream: TcpStream) {
        let ws_stream_res = accept_async(stream).await;
        let ws_stream = match ws_stream_res {
            Ok(ws) => ws,
            Err(e) => {
                error!(
                    "Error getting WebSocketStream for Peer. {:?}, {:?}",
                    peer, e
                );
                return;
            }
        };
        info!("Got WebSocket connection: {:?}", peer);
        ClientConnection::run(ws_stream, peer, self).await;
        info!("WebSocket connection ended: {:?}", peer);
    }
}

type WsStream = WebSocketStream<TcpStream>;
type ArcPlayer = Arc<RwLock<Option<Player>>>;

#[derive(Clone)]
struct ClientConnection {
    write: Arc<Mutex<SplitSink<WsStream, TungMessage>>>,
    com: Arc<CompanionCommunicationHandler>,
    peer: SocketAddr,
    user: Arc<RwLock<Option<(User, AccessToken)>>>,
    voice_state: Arc<RwLock<Option<(GuildId, ChannelId)>>>,
    player_state_sender: Arc<Mutex<Option<JoinHandle<()>>>>,
    voice_state_sender: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ClientConnection {
    pub async fn run(ws: WsStream, peer: SocketAddr, com: CompanionCommunicationHandler) {
        let (tx, rx) = ws.split();
        let handler = Self::new(tx, peer, com);
        handler.receive_run(rx).await
    }

    pub fn new(
        send: SplitSink<WsStream, TungMessage>,
        peer: SocketAddr,
        com: CompanionCommunicationHandler,
    ) -> Self {
        ClientConnection {
            write: Arc::new(Mutex::new(send)),
            com: Arc::new(com),
            peer,
            user: Arc::new(RwLock::new(None)),
            voice_state: Arc::new(RwLock::new(None)),
            player_state_sender: Arc::new(Mutex::new(None)),
            voice_state_sender: Arc::new(Mutex::new(None)),
        }
    }

    async fn receive_run(self, mut receive: SplitStream<WsStream>) {
        while let Some(res) = receive.next().await {
            let msg = match res {
                Ok(m) => m,
                Err(e) => {
                    warn!("WebSocket Receive Error. {:?}, {:?}", self.peer, e);
                    break;
                }
            };
            let bin = match msg {
                TungMessage::Binary(b) => b,
                TungMessage::Close(c) => {
                    info!("Received Close WebSocket Message. {:?}, {:?}", self.peer, c);
                    break;
                }
                _ => continue,
            };
            let msg_res = Message::parse(bin.as_slice());
            let msg = match msg_res {
                Ok(m) => m,
                Err(e) => {
                    warn!("Message Parse Error. {:?}, {:?}", self.peer, e);
                    self.respond(Message::Unexpected(Unexpected::ParseError(
                        bin,
                        format!("{:?}", e),
                    )));
                    continue;
                }
            };
            if let Message::ClientRequest(req) = msg {
                match req {
                    ClientRequest::Authenticate(a) => self.auth(a).await,
                    ClientRequest::AuthStatus() => self.send_auth_status(),
                    ClientRequest::Control(uuid, con) => self.handle_control_req(uuid, con),
                    ClientRequest::End() => {
                        info!("Received End Request. {:?}", self.peer);
                        break;
                    }
                }
            } else {
                warn!("Received Unexpected Message. {:?}, {:?}", self.peer, msg);
                self.respond(Message::Unexpected(Unexpected::MessageType(
                    msg.to_string(),
                )));
            }
        }
        //Closing other threads because receive failed
        if let Some(vss) = self.voice_state_sender.lock().await.take() {
            debug!("Ending Voice State Sender. {:?}", self.peer);
            vss.abort();
        }
        if let Some(pss) = self.player_state_sender.lock().await.take() {
            debug!("Ending Player State Sender. {:?}", self.peer);
            pss.abort();
        }
    }

    fn handle_control_req(&self, uuid: String, con: PlayerControl) {
        info!("Handling Control Request. {:?}, {:?}", self.peer, con);
        let s = self.clone();
        tokio::spawn(async move {
            let mut control_result = PlayerControlResult {
                uuid,
                req: con.clone(),
                res: Ok(()),
            };

            let vs_op = *s.voice_state.read().await;
            let (guild, channel) = match vs_op {
                None => {
                    warn!("There is no player to control. {:?}, {:?}", s.peer, con);
                    control_result.res = Err("No Bot in Channel".to_string());
                    s.sync_respond(Message::ClientControlResult(control_result))
                        .await;
                    return;
                }
                Some(vs) => vs,
            };

            let player_manager_op = s.com.players.get(&guild);
            let player_manager = match player_manager_op {
                None => {
                    error!("Got no Player Manager for Guild. {:?}, {:?}", s.peer, guild);
                    control_result.res = Err("Internal Error".to_string());
                    s.sync_respond(Message::ClientControlResult(control_result))
                        .await;
                    return;
                }
                Some(pm) => pm.clone(),
            };

            let res = match con {
                PlayerControl::Resume() => {
                    player_manager
                        .request(PlayerRequest::PauseResume(channel))
                        .await
                }
                PlayerControl::Pause() => {
                    player_manager
                        .request(PlayerRequest::PauseResume(channel))
                        .await
                }
                PlayerControl::Skip(i) => {
                    player_manager
                        .request(PlayerRequest::Skip(i, channel))
                        .await
                }
                PlayerControl::BackSkip(i) => {
                    player_manager
                        .request(PlayerRequest::BackSkip(i, channel))
                        .await
                }
                PlayerControl::SetTime(pos) => {
                    player_manager
                        .request(PlayerRequest::Jump(pos, channel))
                        .await
                }
                PlayerControl::PlayMode(mode) => {
                    player_manager
                        .request(PlayerRequest::Playback(parse_mode(mode), channel))
                        .await
                }
                PlayerControl::Enqueue(url) => {
                    let res = player_manager.search(channel, url.into()).await;
                    match res {
                        Ok((_, mut tracks)) => {
                            player_manager
                                .request(PlayerRequest::Enqueue(
                                    tracks.drain(..).take(1).collect(),
                                    channel,
                                ))
                                .await
                        }
                        Err(e) => Err(e),
                    }
                }
                PlayerControl::Leave() => player_manager.leave(channel).await,
                PlayerControl::Join() => player_manager.join(channel).await,
            };
            if let Err(e) = res {
                warn!(
                    "Player Control Error. {:?}, {:?}, {:?}, {:?}",
                    s.peer, guild, channel, e
                );
                control_result.res = Err(format!("{:?}", e));
            }
            s.sync_respond(Message::ClientControlResult(control_result))
                .await;
        });
    }

    async fn auth(&self, auth: Auth) {
        //Exchange Token
        let token_res = get_token(auth).await;
        let (access_token, refresh_token) = match token_res {
            Ok(token) => token,
            Err(e) => {
                warn!("Auth Error. {:?}, {:?}", self.peer, e);
                self.clone()
                    .sync_respond(Message::Auth(AuthMessage::AuthError()))
                    .await;
                return;
            }
        };
        //Get User
        let user_res = get_user_id(access_token.clone()).await;
        let user = match user_res {
            Ok(u) => u,
            Err(e) => {
                warn!("Get Client Id Error. {:?}, {:?}", self.peer, e);
                self.clone()
                    .sync_respond(Message::Auth(AuthMessage::AuthError()))
                    .await;
                return;
            }
        };
        //Insert into own Struct
        *self.user.write().await = Some((user.clone(), access_token));

        //Send positive response
        self.clone()
            .sync_respond(Message::Auth(AuthMessage::AuthSuccess(
                user.clone(),
                refresh_token,
            )))
            .await;
        info!("Authenticated User: {:?}, {:?}", self.peer, self.user);

        //remove old voice state sender if it exists
        let mut lock = self.voice_state_sender.lock().await;
        if let Some(vss) = lock.take() {
            //Reset Voice State, Send empty Voice State and Stop Voice State sender
            *self.voice_state.write().await = None;
            self.send_voice_state(None);
            vss.abort();
        }
        //Insert new one
        *lock = Some(tokio::spawn(self.clone().voice_state_sender_run(user)));
        drop(lock)
    }

    fn send_auth_status(&self) {
        let s = self.clone();
        tokio::spawn(async move {
            let user = s.user.read().await.clone();
            s.respond(Message::Auth(AuthMessage::AuthStatus(user.is_some())));
        });
    }

    fn respond(&self, msg: Message) {
        tokio::spawn(self.clone().sync_respond(msg));
    }

    async fn sync_respond(self, msg: Message) {
        let gen_res = msg.generate();
        let bin = match gen_res {
            Ok(b) => b,
            Err(e) => {
                error!("Error Parsing Message. {:?}, {:?}, {:?}", self.peer, msg, e);
                return;
            }
        };

        let res = self.write.lock().await.send(TungMessage::Binary(bin)).await;
        if let Err(e) = res {
            warn!("Send Message Error. {:?}, {:?}", self.peer, e);
        }
    }

    async fn voice_state_sender_run(self, user: User) {
        info!("Starting Voice State Sender Run. {:?}", self.peer);
        let user_id_res = user.id.parse::<u64>();
        let user_id = match user_id_res {
            Ok(id) => UserId(id),
            Err(e) => {
                error!("Parse User ID Error. {:?}, {:?}", self.peer, e);
                return;
            }
        };

        //Make sure, player sender is cleared
        let mut lock = self.player_state_sender.lock().await;
        if let Some(pss) = lock.take() {
            info!("Stopping Player State Sender. {:?}", self.peer);
            pss.abort();
        }
        drop(lock);

        let mut last_check = None;

        loop {
            //If nothing changed: continue
            let new = self.com.bots.get_any_user_voice_channel(&user_id).await;
            if new.eq(&last_check) {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            //Something changed so we drop the current Player State Sender, if it exists
            let mut lock_state_sender = self.player_state_sender.lock().await;
            if let Some(pss) = lock_state_sender.take() {
                info!("Stopping Player State Sender. {:?}", self.peer);
                pss.abort();
            }

            //Replace last check channel, locally and behind the lock
            last_check = new;
            *self.voice_state.write().await = new;

            //Get the new channel or continue if its none
            let (guild, new_channel) = match new {
                None => {
                    //New Channel is none, so we just continue but send the voice_state first
                    self.send_voice_state(None);
                    drop(lock_state_sender);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                Some(ch) => ch,
            };

            //Send new VoiceState
            self.send_voice_state(Some((guild, new_channel)));

            //Starting Player State Sender
            *lock_state_sender = Some(tokio::spawn(
                self.clone().player_state_sender_run(guild, new_channel),
            ));

            drop(lock_state_sender);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn send_voice_state(&self, voice: Option<(GuildId, ChannelId)>) {
        let (guild, channel) = match voice {
            Some(v) => v,
            None => {
                self.respond(Message::UserVoiceState(None));
                return;
            }
        };

        let s = self.clone();
        tokio::spawn(async move {
            //Get any Bot for the Guild
            let bot_op = s.com.bots.get_any_guild_bot(&guild).await;
            let bot = match bot_op {
                Some(b) => b,
                None => {
                    error!("Could not find Bot for Guild. {:?}, {:?}", s.peer, guild);
                    return;
                }
            };
            //Get Channel
            let channel_op = bot.cache().channel(channel).await;
            let channel = match channel_op {
                Some(c) => c,
                None => {
                    error!(
                        "Could not find Channel for Guild. {:?}, Bot: {:?}, {:?}, {:?}",
                        s.peer,
                        bot.id(),
                        guild,
                        channel
                    );
                    return;
                }
            };

            //Build and send Voice State
            let vs = VoiceState {
                channel_id: channel.id().0,
                channel_name: channel
                    .clone()
                    .guild()
                    .map(|c| c.name)
                    .unwrap_or_else(|| channel.to_string()),
            };
            debug!("Sending Voice State. {:?}, {:?}, {:?}", s.peer, guild, vs);
            s.respond(Message::UserVoiceState(Some(vs)));
        });
    }

    async fn player_state_sender_run(self, guild: GuildId, channel: ChannelId) {
        info!("Starting Player State Sender Run. {:?}", self.peer);
        //Get Player Manager for Guild
        let player_manager_op = self.com.players.get(&guild);
        let player_manager = match player_manager_op {
            None => {
                error!(
                    "Got no Player Manager for Guild. {:?}, {:?}",
                    self.peer, guild
                );
                return;
            }
            Some(pm) => pm,
        };

        //Send initial empty state
        if player_manager.get_player(&channel).await.is_none() {
            self.clone().sync_respond(Message::PlayerState(None)).await;
        }

        //Main loop in player state sender run
        'main: loop {
            //Loop until we got a player for our channel
            let (bot, player) = loop {
                //Get Player for Channel
                let player_op = player_manager.get_player(&channel).await;
                if let Some(pair) = player_op {
                    info!(
                        "Got Player for Channel. {:?}, {:?}, {:?}",
                        self.peer, guild, channel
                    );
                    break pair;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            };
            let bot_op = self.com.bots.get_bot_by_id(bot);
            let bot = match bot_op {
                None => {
                    error!(
                        "Could not find Bot. {:?}, {:?}, {:?}",
                        self.peer, guild, bot
                    );
                    continue;
                }
                Some(b) => b.cache().current_user().await,
            };

            //Get watch for player state
            let watch_op = player.read().await.as_ref().map(|p| p.get_status_watch());
            let mut watch = match watch_op {
                Some(p) => p,
                None => {
                    warn!(
                        "First Player read was empty. Starting over. {:?}, {:?}, {:?}",
                        self.peer, guild, channel
                    );
                    continue;
                }
            };

            //Initialize first state
            let mut last_state = gen_player_state(bot.clone(), watch.borrow().clone());
            //And send it synced
            self.clone()
                .sync_respond(Message::PlayerState(Some(State::FullState(
                    last_state.clone(),
                ))))
                .await;

            loop {
                let watch_res = watch.changed().await;
                if let Err(e) = watch_res {
                    info!(
                        "Player Watch Ended. {:?}, {:?}, {:?}, {:?}",
                        self.peer, guild, channel, e
                    );
                    self.clone()
                        .sync_respond(Message::PlayerState(Some(State::EmptyState())))
                        .await;
                    continue 'main;
                }

                //Get new State
                let new_state = gen_player_state(bot.clone(), watch.borrow().clone());
                //If State did not change, wait for next change
                if new_state.eq(&last_state) {
                    continue;
                }
                //Generate Patch
                let patch_res = Message::generate_patch(&last_state, &new_state);
                let patch = match patch_res {
                    Ok(p) => p,
                    Err(e) => {
                        error!(
                            "Error Generating Patch. {:?}, {:?}, {:?}, {:?}",
                            self.peer, guild, channel, e
                        );
                        continue;
                    }
                };
                last_state = new_state;

                debug!("Sending Patch. {:?}, {:?}, {:?}", self.peer, guild, channel);
                self.clone()
                    .sync_respond(Message::PlayerState(Some(State::UpdateState(patch))))
                    .await;
            }
        }
    }
}

fn gen_player_state(bot: CurrentUser, ps: Arc<crate::player::PlayerState>) -> Box<PlayerState> {
    let current = ps
        .current
        .as_ref()
        .map(|((pos, when), track)| parse_cur_track(pos, when, track))
        .flatten();
    let history: Vec<_> = ps
        .history
        .iter()
        .map(|t| parse_track(t))
        .flatten()
        .collect();
    let playlist: Vec<_> = ps
        .playlist
        .iter()
        .map(|t| parse_track(t))
        .flatten()
        .collect();

    let new_ps = PlayerState {
        bot: BotInfo {
            name: bot.name.clone(),
            avatar: bot.avatar_url().unwrap_or_else(|| bot.default_avatar_url()),
        },
        paused: ps.play_state.is_paused(),
        mode: ps.playback.into(),
        current,
        history,
        queue: playlist,
    };
    Box::new(new_ps)
}

fn parse_track(t: &lavalink_rs::model::Track) -> Option<Track> {
    let info = t.info.clone()?;

    Some(Track {
        len: Duration::from_millis(info.length),
        pos: Duration::from_millis(info.position),
        title: info.title,
        uri: info.uri,
    })
}

fn parse_cur_track(pos: &Duration, when: &Instant, t: &lavalink_rs::model::Track) -> Option<Track> {
    let info = t.info.clone()?;

    Some(Track {
        len: Duration::from_millis(info.length),
        pos: *pos + when.elapsed(),
        title: info.title,
        uri: info.uri,
    })
}

fn parse_mode(pm: PlayMode) -> crate::player::Playback {
    match pm {
        PlayMode::Normal => crate::player::Playback::Normal,
        PlayMode::LoopAll => crate::player::Playback::AllLoop,
        PlayMode::LoopOne => crate::player::Playback::OneLoop,
    }
}

impl From<crate::player::Playback> for PlayMode {
    fn from(p: crate::player::Playback) -> Self {
        match p {
            crate::player::Playback::Normal => PlayMode::Normal,
            crate::player::Playback::AllLoop => PlayMode::LoopAll,
            crate::player::Playback::OneLoop => PlayMode::LoopOne,
        }
    }
}

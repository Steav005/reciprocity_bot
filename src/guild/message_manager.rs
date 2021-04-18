use crate::bots::Bot;
use crate::context::{Context, GuildEventHandler};
use crate::guild::scheduler::GuildScheduler;
use crate::guild::ReciprocityGuild;
use crate::task_handle::{AddMessageReactionTask, DeleteMessageReactionTask, DeleteMessageTask};
use futures::{Future, FutureExt};
use lavalink_rs::model::Track;
use log::{info, warn};
use serenity::client::bridge::gateway::ShardMessenger;
use serenity::collector::ReactionAction;
use serenity::http::Http;
use serenity::model::prelude::{
    ChannelId, GuildId, Message, MessageId, Reaction, ReactionType, User, UserId,
};
use serenity::prelude::SerenityError;
use serenity::utils::MessageBuilder;
use std::borrow::Borrow;
use std::convert::AsRef;
use std::convert::{TryFrom, TryInto};
use std::fmt::Write;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

const DELETE_MESSAGE_DELAY: Duration = Duration::from_millis(500);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
const SEARCH_TITLE_LIMIT: usize = 40;
const MESSAGE_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

///Adds a list of emotes to a message
async fn add_emotes(
    message: MessageId,
    channel: ChannelId,
    emotes: Arc<Vec<EmoteAction>>,
    scheduler: GuildScheduler,
) {
    for task in emotes.iter().map(|emote| AddMessageReactionTask {
        channel,
        message,
        reaction: emote.reaction(),
    }) {
        //Add single emote and wait for completion
        if let Err(e) = scheduler.process(task).await {
            warn!(
                "Error Adding Emote to Search Message: {:?}, Error: {:?}",
                message, e
            );
        }
    }
}

pub struct SearchMessage;

impl SearchMessage {
    pub async fn search(
        bot: Arc<Http>,
        tracks: Vec<Track>,
        requester: User,
        query: String,
        shard_messenger: impl AsRef<ShardMessenger>,
        context: Context,
    ) -> Result<Track, MessageError> {
        info!(
            "New Search Message. Guild: {:?}, User: {:?}, Query: {:?}",
            context.id, requester.id, query
        );
        let message = context
            .channel
            .send_message(bot, |m| {
                m.content(Self::content(tracks.as_slice(), &query, &requester))
            })
            .await
            .map_err(MessageError::SerenityError)?;

        //Delete old message and add new id to the message map
        let mut messages_lock = context.search_messages.write().await;
        if let Some(old_id) = messages_lock.get(&requester.id) {
            let delete_task = DeleteMessageTask {
                channel: context.channel,
                message: *old_id,
            };
            context.scheduler.process_enqueue(delete_task).await.ok();
        }
        messages_lock.insert(requester.id, message.id);
        drop(messages_lock);

        //Build emotes, that we are using with this message
        let emotes: Arc<Vec<_>> = Arc::new(
            (1..tracks.len())
                .take(10)
                .map(EmoteAction::Number)
                .chain(vec![EmoteAction::Delete()])
                .collect(),
        );
        let emotes_1 = emotes.clone();

        let filter =
            move |r: &Arc<Reaction>| emotes.iter().any(|e| r.emoji.unicode_eq(e.unicode()));
        let collector = message
            .await_reaction(&shard_messenger)
            .timeout(SEARCH_TIMEOUT)
            .author_id(requester.id.0)
            .removed(false)
            .added(true)
            .filter(filter)
            //Map into EmoteAction
            .map(|r| EmoteAction::try_from(r?.as_inner_ref().deref()).ok())
            //Map into Track
            .map(move |e| {
                if let Some(EmoteAction::Number(i)) = e {
                    return tracks.get(i - 1).cloned();
                }
                None
            });
        tokio::spawn(add_emotes(
            message.id,
            context.channel,
            emotes_1.clone(),
            context.scheduler.clone(),
        ));
        let track: Result<Track, MessageError> = collector.await.ok_or(MessageError::Timeout());

        context
            .scheduler
            .process_enqueue(DeleteMessageTask {
                channel: context.channel,
                message: message.id,
            })
            .await
            .ok();

        track
    }

    fn content(tracks: &[Track], query: &str, requester: &User) -> String {
        let mut content = format!("[{}] {:.*}\r\n", query, 16, requester.name);
        for (i, track) in tracks.iter().enumerate().take(10) {
            write!(
                content,
                "{}: {:.*}\r\n",
                i + 1,
                SEARCH_TITLE_LIMIT,
                track
                    .clone()
                    .info
                    .map_or("Missing Name".to_string(), |info| info.title)
            )
            .unwrap()
        }

        let content = MessageBuilder::new()
            .push_codeblock(content, Some("cs"))
            .build();
        content
    }
}

#[derive(Clone)]
pub struct MainMessage {
    lock: Arc<Mutex<()>>,
    message: Message,
    bot: Arc<Bot>,
    shard: ShardMessenger,
    context: Context,
}

impl MainMessage {
    const EMOTES: [EmoteAction; 7] = [
        EmoteAction::Prev(),
        EmoteAction::PlayPause(),
        EmoteAction::Next(),
        EmoteAction::LoopOne(),
        EmoteAction::LoopAll(),
        EmoteAction::Join(),
        EmoteAction::Leave(),
    ];

    pub async fn new(
        guild: ReciprocityGuild,
        context: Context,
    ) -> Result<(Self, impl Future<Output = ()>), MessageError> {
        info!("Start new Main Message. Guild: {:?}", context.id);
        let bot = context
            .bots
            .get_any_guild_bot(&context.id)
            .await
            .ok_or(MessageError::NoBot(context.id))?;
        let shard = context
            .event_handler
            .get_shard_sender(context.id, bot.id())
            .await
            .ok_or_else(|| MessageError::NoShard(bot.id()))?;

        let content = Self::content(&context).await;
        let message = context
            .channel
            .send_message(bot.http(), |m| m.content(content))
            .await
            .map_err(MessageError::SerenityError)?;
        let main_message = MainMessage {
            lock: Arc::new(Mutex::new(())),
            message,
            bot,
            shard,
            context,
        };
        tokio::spawn(main_message.clone().update());
        Ok((main_message.clone(), main_message.run(guild)))
    }

    pub async fn run(self, guild: ReciprocityGuild) {
        tokio::spawn(self.clone().emote_check());

        let mut collector = self
            .message
            .await_reactions(&self.shard)
            .added(true)
            .removed(true)
            .await;

        info!(
            "Starting Message Collector for Main Message: {:?}, Guild: {:?}",
            self.message.id, self.context.id
        );
        while let Some(reaction) = collector.next().await {
            match reaction.deref() {
                ReactionAction::Added(reaction) => {
                    if let Some(user) = &reaction.user_id {
                        if self.context.bots.contains_id(user) {
                            continue;
                        }
                        // Pass on Event to Guild
                        if let Ok(emote_action) = reaction.deref().try_into() {
                            let cloned_user = *user;
                            let cloned_guild = guild.clone();
                            tokio::spawn(async move {
                                cloned_guild
                                    .main_message_event(emote_action, cloned_user)
                                    .await
                            });
                        }

                        // Delete Reaction
                        self.context
                            .scheduler
                            .process_enqueue(DeleteMessageReactionTask {
                                channel: reaction.channel_id,
                                message: reaction.message_id,
                                user: *user,
                                reaction: reaction.emoji.clone(),
                            })
                            .await
                            .ok();
                    } else {
                        // Check Message
                        tokio::spawn(self.clone().emote_check());
                    }
                }
                ReactionAction::Removed(reaction) => {
                    if let Some(user) = &reaction.user_id {
                        if !self.context.bots.contains_id(user) {
                            continue;
                        }
                    }
                    // Check Message
                    tokio::spawn(self.clone().emote_check());
                }
            }
        }
    }

    pub async fn message_still_exists(&self) -> bool {
        let msg = self
            .bot
            .http()
            .get_message(self.message.channel_id.0, self.message.id.0)
            .await;
        msg.is_ok()
    }

    pub fn message_id(&self) -> MessageId {
        self.message.id
    }

    pub fn channel_id(&self) -> ChannelId {
        self.message.channel_id
    }

    async fn update(self) {
        let mut message = self.message;
        loop {
            tokio::time::sleep(MESSAGE_UPDATE_INTERVAL).await;
            let content = Self::content(&self.context).await;
            if message.content.eq(content.as_str()) {
                continue;
            }
            info!(
                "Updating Message: {:?}, Guild: {:?}",
                message.id, self.context.id
            );
            let edit_res = message
                .edit(self.bot.cache_http(), |msg| msg.content(content))
                .await;
            if let Err(e) = edit_res {
                //BREAK Update Loop if error occurred
                warn!(
                    "Error updating Message: {:?}, Guild: {:?}, Error: {:?}",
                    message.id, self.context.id, e
                );
                return;
            }
        }
    }

    pub async fn emote_check(self) {
        let lock = self.lock.lock().await;

        //Get fresh msg
        let msg = match self
            .bot
            .http()
            .get_message(self.message.channel_id.0, self.message.id.0)
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                warn!("Error getting Message for emote check. Guild: {:?}, Message: {:?}, Error: {:?}", self.context.id, self.message.id, e);
                return;
            }
        };

        //Check if any reaction is missing
        let missing_reaction = Self::EMOTES.iter().any(|e| {
            !msg.reactions
                .iter()
                .any(|r| r.reaction_type.unicode_eq(e.unicode()))
        });
        if !missing_reaction {
            return;
        }

        info!(
            "Message is missing emote: {:?}, {:?}",
            self.context.id, msg.id
        );

        //Delete all Reactions
        let delete_all_res = msg.delete_reactions(self.bot.cache_http()).await;
        if let Err(e) = delete_all_res {
            warn!(
                "Error deleting Reactions for Message: {:?}, Error: {:?}",
                msg.id, e
            );
            return;
        }

        //Add Reactions one after another
        for e in Self::EMOTES.iter() {
            let task = AddMessageReactionTask {
                channel: msg.channel_id,
                message: msg.id,
                reaction: e.reaction(),
            };
            let add_res = self.context.scheduler.process(task).await;
            if let Err(e) = add_res {
                warn!(
                    "Error adding Reaction to Message: {:?}, Error: {:?}",
                    msg.id, e
                );
                return;
            }
        }

        drop(lock)
    }

    fn duration_fmt(dur: &'_ Duration) -> String {
        let seconds = dur.as_secs() % 60;
        let minutes = (dur.as_secs() / 60) % 60;
        let hours = (dur.as_secs() / 60) / 60;
        let mut msg = String::from("");
        if hours > 0 {
            write!(msg, "{:02}:", hours).unwrap();
        }
        write!(msg, "{:02}:{:02}", minutes, seconds).unwrap();
        msg
    }

    async fn content(context: &Context) -> String {
        let mut msg: String = "```cs\r\n".to_string();
        let states = context.player_manager.get_all_player_states().await;
        let mut active_player = 0;

        for state in states.iter().map(|s| s.borrow().clone()) {
            if let Some(bot) = context.bots.get_bot_by_id(state.bot) {
                if let Some(bot) = bot.cache().member(context.id, state.bot).await {
                    active_player += 1;

                    write!(msg, "[{}]", bot.nick.unwrap_or(bot.user.name)).unwrap();

                    if state.current.is_none() && state.playlist.is_empty() {
                        write!(msg, " No Song in Playlist\r\n").unwrap();
                    } else {
                        write!(msg, " {}\r\n", state.playback.to_string()).unwrap();
                    }

                    if let Some(((dur, when), cur)) = &state.current {
                        write!(
                            msg,
                            "{:.*} [{}/{}]\r\n",
                            SEARCH_TITLE_LIMIT,
                            cur.info
                                .clone()
                                .map_or("No Track Name".to_string(), |i| i.title),
                            Self::duration_fmt(&(when.elapsed() + *dur)),
                            cur.info
                                .clone()
                                .map_or("--:--".to_string(), |i| Self::duration_fmt(
                                    &Duration::from_millis(i.length)
                                ))
                        )
                        .unwrap();
                    }
                    for (i, track) in state.playlist.iter().enumerate().take(2) {
                        write!(
                            msg,
                            "[{}] {:.*}\r\n",
                            i + 1,
                            SEARCH_TITLE_LIMIT,
                            track
                                .info
                                .clone()
                                .map_or("No Track Name".to_string(), |i| i.title)
                        )
                        .unwrap();
                    }
                    write!(msg, "\r\n").unwrap();
                }
            }
        }

        //If there are no active Player
        if active_player == 0 {
            write!(msg, "[No active Player]").unwrap();
        }

        write!(msg, "```").unwrap();
        msg
    }
}

#[derive(Error, Debug)]
pub enum MessageError {
    #[error("Serenity Error occurred: {0:?}")]
    SerenityError(SerenityError),
    #[error("Could not find Bot for Guild: {0:?}")]
    NoBot(GuildId),
    #[error("Could not find Shard for Bot: {0:?}")]
    NoShard(UserId),
    #[error("Message Timeout")]
    Timeout(),
    #[error("Unexpectedly ended")]
    UnexpectedEnd(),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EmoteAction {
    Number(usize),
    PlayPause(),
    Next(),
    Prev(),
    Join(),
    Leave(),
    Delete(),
    LoopOne(),
    LoopAll(),
    Nothing(),
}

impl EmoteAction {
    const NUMBERS: [&'static str; 11] = ["0️⃣", "1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣", "🔟"];
    const PLAY_PAUSE: &'static str = "⏯";
    const NEXT: &'static str = "⏭";
    const PREV: &'static str = "⏮";
    const JOIN: &'static str = "📥";
    const LEAVE: &'static str = "📤";
    const DELETE: &'static str = "❌";
    const LOOP_ONE: &'static str = "🔂";
    const LOOP_ALL: &'static str = "🔁";
    const NOTHING: &'static str = "無";

    pub fn unicode(&self) -> &str {
        match self {
            EmoteAction::Number(number) => Self::NUMBERS.get(*number).unwrap_or(&""),
            EmoteAction::PlayPause() => Self::PLAY_PAUSE,
            EmoteAction::Next() => Self::NEXT,
            EmoteAction::Prev() => Self::PREV,
            EmoteAction::Join() => Self::JOIN,
            EmoteAction::Leave() => Self::LEAVE,
            EmoteAction::Delete() => Self::DELETE,
            EmoteAction::LoopOne() => Self::LOOP_ONE,
            EmoteAction::LoopAll() => Self::LOOP_ALL,
            EmoteAction::Nothing() => Self::NOTHING,
        }
    }

    pub fn reaction(&self) -> ReactionType {
        ReactionType::Unicode(self.unicode().to_string())
    }
}

impl TryFrom<&Reaction> for EmoteAction {
    type Error = ();

    fn try_from(value: &Reaction) -> Result<Self, Self::Error> {
        if let ReactionType::Unicode(str) = value.emoji.borrow() {
            return EmoteAction::try_from(str.as_str());
        }
        Err(())
    }
}

impl TryFrom<&str> for EmoteAction {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            Self::PLAY_PAUSE => Ok(Self::PlayPause()),
            Self::NEXT => Ok(Self::Next()),
            Self::PREV => Ok(Self::Prev()),
            Self::JOIN => Ok(Self::Join()),
            Self::LEAVE => Ok(Self::Leave()),
            Self::DELETE => Ok(Self::Delete()),
            Self::LOOP_ONE => Ok(Self::LoopOne()),
            Self::LOOP_ALL => Ok(Self::LoopAll()),
            Self::NOTHING => Ok(Self::Nothing()),
            _ => {
                if let Some(i) = Self::NUMBERS.iter().position(|n| value.eq(*n)) {
                    Ok(Self::Number(i))
                } else {
                    Err(())
                }
            }
        }
    }
}

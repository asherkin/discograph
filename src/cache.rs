use anyhow::{Context, Result};
use futures::future::join_all;
use lru::LruCache;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};
use twilight_gateway::MessageSender;
use twilight_http::Client;
use twilight_model::channel::message::{Mention, MessageType};
use twilight_model::channel::{Channel, ChannelType, Message};
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::{MemberUpdate, MessageUpdate};
use twilight_model::gateway::payload::outgoing::RequestGuildMembers;
use twilight_model::guild::{Guild, Member, PartialGuild, PartialMember, Permissions, Role};
use twilight_model::id::marker::{
    ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker,
};
use twilight_model::id::Id;
use twilight_model::user::User;
use twilight_model::util::ImageHash;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CachedUser {
    pub id: Id<UserMarker>,
    pub name: String,
    pub discriminator: u16,
    pub avatar: Option<ImageHash>,
    pub bot: bool,
}

impl From<&User> for CachedUser {
    fn from(user: &User) -> Self {
        CachedUser {
            id: user.id,
            name: user.name.clone(),
            discriminator: user.discriminator,
            avatar: user.avatar,
            bot: user.bot,
        }
    }
}

impl From<&Mention> for CachedUser {
    fn from(mention: &Mention) -> Self {
        CachedUser {
            id: mention.id,
            name: mention.name.clone(),
            discriminator: mention.discriminator,
            avatar: mention.avatar,
            bot: mention.bot,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedGuild {
    pub id: Id<GuildMarker>,
    pub name: String,
    pub icon: Option<ImageHash>,
    pub roles: Vec<Id<RoleMarker>>,
    pub owner_id: Id<UserMarker>,
}

impl From<&PartialGuild> for CachedGuild {
    fn from(guild: &PartialGuild) -> Self {
        CachedGuild {
            id: guild.id,
            name: guild.name.clone(),
            icon: guild.icon,
            roles: guild.roles.iter().map(|role| role.id).collect(),
            owner_id: guild.owner_id,
        }
    }
}

impl From<&Guild> for CachedGuild {
    fn from(guild: &Guild) -> Self {
        CachedGuild {
            id: guild.id,
            name: guild.name.clone(),
            icon: guild.icon,
            roles: guild.roles.iter().map(|role| role.id).collect(),
            owner_id: guild.owner_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedRole {
    pub id: Id<RoleMarker>,
    pub name: String,
    pub color: u32,
    pub position: i64,
    pub permissions: Permissions,
}

impl From<&Role> for CachedRole {
    fn from(role: &Role) -> Self {
        CachedRole {
            id: role.id,
            name: role.name.clone(),
            color: role.color,
            position: role.position,
            permissions: role.permissions,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedMember {
    pub nick: Option<String>,
    pub avatar: Option<ImageHash>,
    pub roles: Vec<Id<RoleMarker>>,
}

impl From<&PartialMember> for CachedMember {
    fn from(member: &PartialMember) -> Self {
        CachedMember {
            nick: member.nick.clone(),
            avatar: member.avatar,
            roles: member.roles.clone(),
        }
    }
}

impl From<&Member> for CachedMember {
    fn from(member: &Member) -> Self {
        CachedMember {
            nick: member.nick.clone(),
            avatar: member.avatar,
            roles: member.roles.clone(),
        }
    }
}

impl From<&MemberUpdate> for CachedMember {
    fn from(member: &MemberUpdate) -> Self {
        CachedMember {
            nick: member.nick.clone(),
            avatar: member.avatar,
            roles: member.roles.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedChannel {
    pub id: Id<ChannelMarker>,
    pub name: String,
    pub kind: ChannelType,
}

impl From<&Channel> for CachedChannel {
    fn from(channel: &Channel) -> Self {
        CachedChannel {
            id: channel.id,
            name: channel.name.as_ref().map_or_else(
                || format!("{:?}:{}", channel.kind, channel.id),
                |name| name.clone(),
            ),
            kind: channel.kind,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedMessage {
    pub author_id: Id<UserMarker>,
    pub kind: MessageType,
}

impl From<&Message> for CachedMessage {
    fn from(message: &Message) -> Self {
        CachedMessage {
            author_id: message.author.id,
            kind: message.kind,
        }
    }
}

// TODO: The locking strategy in these two structs is awful and doesn't align with the usage pattern
//       at all. Once we're ready to clean up Cache again should strip it all out and redesign what
//       needs sharing, our guild cache in particular is likely quite bad. We've also now gotten
//       LruCache out of most places, which means we can use RwLock instead of Mutex.

struct GuildCache {
    guild: Mutex<CachedGuild>,
    roles: Mutex<HashMap<Id<RoleMarker>, CachedRole>>,
    members: Mutex<LruCache<Id<UserMarker>, CachedMember>>,
    channels: Mutex<HashMap<Id<ChannelMarker>, CachedChannel>>,
    /// Used to lookup the author of messages being reacted to.
    messages: Mutex<LruCache<Id<MessageMarker>, CachedMessage>>,
}

pub struct Cache {
    http: Arc<Client>,
    pending_guild_members: Mutex<HashMap<String, mpsc::Sender<Vec<Id<UserMarker>>>>>,
    users: Mutex<LruCache<Id<UserMarker>, CachedUser>>,
    guilds: Mutex<HashMap<Id<GuildMarker>, Arc<GuildCache>>>,
}

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub struct CacheStats {
    users: usize,
    guilds: usize,
    roles: usize,
    members: usize,
    channels: usize,
    messages: usize,
}

// TODO: Consider being more selective about who gets cached.
const USERS_LRU_CACHE_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(5000) };

// TODO: Consider being more selective about who gets cached.
const MEMBERS_LRU_CACHE_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(500) };

// TODO: Consider splitting off a separate one for recent messages and looked up messages.
const MESSAGES_LRU_CACHE_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(100) };

// The `get_*` functions in here release the lock while processing in order to support async in
// the future, and a potential switch to RwLock if we move away from LruCache.
impl Cache {
    pub fn new(http: Arc<Client>) -> Self {
        Cache {
            http,
            pending_guild_members: Mutex::new(HashMap::new()),
            users: Mutex::new(LruCache::new(USERS_LRU_CACHE_LIMIT)),
            guilds: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_stats(&self) -> CacheStats {
        let mut guilds = 0;
        let mut roles = 0;
        let mut members = 0;
        let mut channels = 0;
        let mut messages = 0;

        for guild in self.guilds.lock().values() {
            guilds += 1;
            roles += guild.roles.lock().len();
            members += guild.members.lock().len();
            channels += guild.channels.lock().len();
            messages += guild.messages.lock().len();
        }

        CacheStats {
            users: self.users.lock().len(),
            guilds,
            roles,
            members,
            channels,
            messages,
        }
    }

    pub fn get_guild_count(&self) -> usize {
        self.guilds.lock().len()
    }

    pub fn update(&self, event: &Event) {
        match event {
            Event::Ready(ready) => {
                let shard = ready.shard.expect("expected a shard id in ready event");

                // Remove all the guilds in our cache that belonged to this shard ID.
                // This assumes the shard count wont change while running, but that's currently true.
                // We can't do this on disconnect as if we get a resume we still need the data.
                self.guilds.lock().retain(|guild_id, _| {
                    ((guild_id.get() >> 22) % shard.total()) != shard.number()
                });
            }
            Event::ChannelCreate(channel) => self.put_channel(channel),
            Event::ChannelUpdate(channel) => self.put_channel(channel),
            Event::ChannelDelete(channel) => {
                if let Some(guild_id) = &channel.guild_id {
                    self.guilds
                        .lock()
                        .get_mut(guild_id)
                        .and_then(|guild| guild.channels.lock().remove(&channel.id));
                } else {
                    let channel = &***channel;
                    warn!(?channel, "cache got a non-guild channel deletion");
                }
            }
            Event::ThreadCreate(thread) => self.put_channel(thread),
            Event::ThreadUpdate(thread) => self.put_channel(thread),
            Event::ThreadDelete(thread) => {
                self.guilds
                    .lock()
                    .get_mut(&thread.guild_id)
                    .and_then(|guild| guild.channels.lock().remove(&thread.id));
            }
            Event::GuildCreate(guild) => self.put_full_guild(guild),
            Event::GuildUpdate(guild) => self.put_guild(guild),
            Event::GuildDelete(guild) => {
                self.guilds.lock().remove(&guild.id);
            }
            Event::MemberAdd(member) => self.put_full_member(member.guild_id, member),
            Event::MemberUpdate(member) => self.put_member_update(member),
            Event::MemberChunk(chunk) => {
                for member in &chunk.members {
                    self.put_full_member(chunk.guild_id, member)
                }

                if let Some(nonce) = &chunk.nonce {
                    let mut pending_guild_members = self.pending_guild_members.lock();

                    let nonce = nonce.clone();
                    let not_found = chunk.not_found.clone();

                    let to_complete = pending_guild_members.remove(&nonce);
                    if to_complete.is_none() {
                        debug!(
                            "cache got member chunk with nonce {}, but there was no pending request",
                            nonce
                        );
                    }

                    // Ping an empty list to everyone else waiting to let them know we're processing.
                    let to_notify: Vec<_> = pending_guild_members.values().cloned().collect();

                    tokio::spawn(async move {
                        if let Some(to_complete) = to_complete {
                            if to_complete.send(not_found).await.is_err() {
                                warn!("failed to notify member chunk with nonce {}", nonce);
                            }
                        }

                        join_all(to_notify.iter().map(|sender| sender.send(Vec::new()))).await;
                    });
                }
            }
            Event::MemberRemove(member) => {
                self.put_user(&member.user);

                self.guilds
                    .lock()
                    .get_mut(&member.guild_id)
                    .and_then(|guild| guild.members.lock().pop(&member.user.id));
            }
            Event::MessageCreate(message) => self.put_message(message),
            Event::MessageUpdate(message) => self.put_message_update(message),
            Event::MessageDelete(message) => {
                if let Some(guild_id) = &message.guild_id {
                    self.guilds
                        .lock()
                        .get_mut(guild_id)
                        .and_then(|guild| guild.messages.lock().pop(&message.id));
                } else {
                    warn!(?message, "cache got a non-guild message deletion");
                }
            }
            Event::ReactionAdd(reaction) => {
                if let (Some(guild_id), Some(member)) = (reaction.guild_id, &reaction.member) {
                    self.put_full_member(guild_id, member);
                }
            }
            Event::ReactionRemove(reaction) => {
                if let (Some(guild_id), Some(member)) = (reaction.guild_id, &reaction.member) {
                    self.put_full_member(guild_id, member);
                }
            }
            Event::RoleCreate(role) => self.put_role(role.guild_id, &role.role),
            Event::RoleUpdate(role) => self.put_role(role.guild_id, &role.role),
            Event::RoleDelete(role) => {
                self.guilds
                    .lock()
                    .get_mut(&role.guild_id)
                    .and_then(|guild| guild.roles.lock().remove(&role.role_id));
            }
            Event::InteractionCreate(interaction) => {
                if let Some(message) = &interaction.message {
                    self.put_message(message);
                }

                if let Some(user) = &interaction.user {
                    self.put_user(user);
                }

                if let (Some(guild_id), Some(member)) = (interaction.guild_id, &interaction.member)
                {
                    if let Some(user) = &member.user {
                        self.put_user(user);

                        self.put_member(guild_id, user.id, member);
                    }
                }
            }
            _ => info!("event not used by cache: {:?}", event.kind()),
        }

        // debug!("cache stats: {:?}", self.get_stats());
    }

    /// This is like bulk_request_members, but only loads members not in the cache.
    ///
    /// Returns a tuple of users that were already loaded, and users that could not be loaded.
    pub async fn bulk_preload_members(
        &self,
        shard: &MessageSender,
        guild_id: Id<GuildMarker>,
        user_ids: impl Iterator<Item = Id<UserMarker>>,
    ) -> Result<(Vec<Id<UserMarker>>, Vec<Id<UserMarker>>)> {
        let (already_loaded, users_to_load): (Vec<_>, Vec<_>) = {
            match self.guilds.lock().get_mut(&guild_id) {
                Some(guild) => {
                    let members = guild.members.lock();
                    user_ids.partition(|user_id| members.contains(user_id))
                }
                None => (Vec::new(), user_ids.collect()),
            }
        };

        if users_to_load.is_empty() {
            return Ok((already_loaded, Vec::new()));
        }

        Ok((
            already_loaded,
            self.bulk_request_members(shard, guild_id, &users_to_load)
                .await?,
        ))
    }

    /// Bulk request member info for a guild via the gateway.
    ///
    /// Returns the IDs of members that could not be loaded by Discord.
    pub async fn bulk_request_members(
        &self,
        shard: &MessageSender,
        guild_id: Id<GuildMarker>,
        user_ids: &[Id<UserMarker>],
    ) -> Result<Vec<Id<UserMarker>>> {
        let requests: Vec<_> = user_ids
            .chunks(100)
            .map(|chunk| {
                use rand::distributions::Alphanumeric;
                use rand::{thread_rng, Rng};

                let nonce: String = thread_rng()
                    .sample_iter(&Alphanumeric)
                    .take(32)
                    .map(char::from)
                    .collect();

                let command = RequestGuildMembers::builder(guild_id)
                    .nonce(&nonce)
                    .user_ids(chunk)
                    .unwrap();

                (nonce, command)
            })
            .collect();

        info!(
            "requesting {} members for guild {} in {} chunks",
            user_ids.len(),
            guild_id,
            requests.len(),
        );

        let mut rx = {
            let mut pending_guild_members = self.pending_guild_members.lock();

            let (tx, rx) = mpsc::channel(10);

            for (nonce, command) in &requests {
                if pending_guild_members
                    .insert(nonce.clone(), tx.clone())
                    .is_some()
                {
                    panic!("guild member chunk request nonce collision occurred");
                }

                if let Err(error) = shard.command(command) {
                    warn!(?command, ?error, "failed to request member chunk");

                    pending_guild_members.remove(nonce);
                }
            }

            drop(tx);

            rx
        };

        let mut not_found = Vec::new();

        loop {
            debug!("polling member responses for guild {}", guild_id);

            match timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(None) => {
                    info!("all members received for guild {}", guild_id);

                    break;
                }
                Ok(Some(results)) => {
                    // Empty results are used to reset our timeout.
                    if !results.is_empty() {
                        not_found.extend(results.into_iter());
                    }
                }
                Err(_) => {
                    warn!("member chunk request for guild {} timed out", guild_id);

                    let mut pending_guild_members = self.pending_guild_members.lock();

                    for (nonce, _) in &requests {
                        pending_guild_members.remove(nonce);
                    }

                    break;
                }
            }
        }

        Ok(not_found)
    }

    fn put_user(&self, user: &User) {
        let mut cache = self.users.lock();
        cache.put(user.id, CachedUser::from(user));
    }

    fn put_user_mention(&self, guild_id: Option<Id<GuildMarker>>, mention: &Mention) {
        let mut cache = self.users.lock();
        cache.put(mention.id, CachedUser::from(mention));

        if let (Some(guild_id), Some(member)) = (guild_id, &mention.member) {
            self.put_member(guild_id, mention.id, member);
        }
    }

    pub async fn get_user(&self, user_id: Id<UserMarker>) -> Result<CachedUser> {
        let cached_user = {
            let mut cache = self.users.lock();
            cache.get(&user_id).cloned()
        };

        match cached_user {
            Some(cached_user) => Ok(cached_user),
            None => {
                info!("user {} not in cache, fetching", user_id);

                let user = self.http.user(user_id).await?.model().await?;

                self.put_user(&user);

                Ok(CachedUser::from(&user))
            }
        }
    }

    fn put_guild(&self, guild: &PartialGuild) {
        let mut cache = self.guilds.lock();
        cache
            .entry(guild.id)
            .and_modify(|cache| {
                {
                    let mut old_guild = cache.guild.lock();
                    *old_guild = CachedGuild::from(guild);
                }

                {
                    let mut old_roles = cache.roles.lock();
                    *old_roles = guild
                        .roles
                        .iter()
                        .map(|role| (role.id, CachedRole::from(role)))
                        .collect();
                }
            })
            .or_insert_with(|| {
                Arc::new(GuildCache {
                    guild: Mutex::new(CachedGuild::from(guild)),
                    roles: Mutex::new(
                        guild
                            .roles
                            .iter()
                            .map(|role| (role.id, CachedRole::from(role)))
                            .collect(),
                    ),
                    members: Mutex::new(LruCache::new(MEMBERS_LRU_CACHE_LIMIT)),
                    channels: Mutex::new(HashMap::new()),
                    messages: Mutex::new(LruCache::new(MESSAGES_LRU_CACHE_LIMIT)),
                })
            });
    }

    fn put_full_guild(&self, guild: &Guild) {
        let mut cache = self.guilds.lock();
        cache
            .entry(guild.id)
            .and_modify(|cache| {
                {
                    let mut old_guild = cache.guild.lock();
                    *old_guild = CachedGuild::from(guild);
                }

                {
                    let mut old_roles = cache.roles.lock();
                    *old_roles = guild
                        .roles
                        .iter()
                        .map(|role| (role.id, CachedRole::from(role)))
                        .collect();
                }

                {
                    let mut old_channels = cache.channels.lock();
                    *old_channels = guild
                        .channels
                        .iter()
                        .map(|channel| (channel.id, CachedChannel::from(channel)))
                        .collect();
                }
            })
            .or_insert_with(|| {
                Arc::new(GuildCache {
                    guild: Mutex::new(CachedGuild::from(guild)),
                    roles: Mutex::new(
                        guild
                            .roles
                            .iter()
                            .map(|role| (role.id, CachedRole::from(role)))
                            .collect(),
                    ),
                    members: Mutex::new(LruCache::new(MEMBERS_LRU_CACHE_LIMIT)),
                    channels: Mutex::new(
                        guild
                            .channels
                            .iter()
                            .map(|channel| (channel.id, CachedChannel::from(channel)))
                            .collect(),
                    ),
                    messages: Mutex::new(LruCache::new(MESSAGES_LRU_CACHE_LIMIT)),
                })
            });
    }

    pub async fn get_guild(&self, guild_id: Id<GuildMarker>) -> Result<CachedGuild> {
        let cached_guild = {
            let cache = self.guilds.lock();
            cache.get(&guild_id).map(|guild| guild.guild.lock().clone())
        };

        match cached_guild {
            Some(cached_guild) => Ok(cached_guild),
            None => {
                info!("guild {} not in cache, fetching", guild_id);

                let guild = self.http.guild(guild_id).await?.model().await?;

                self.put_full_guild(&guild);

                Ok(CachedGuild::from(&guild))
            }
        }
    }

    fn put_role(&self, guild_id: Id<GuildMarker>, role: &Role) {
        self.guilds
            .lock()
            .get_mut(&guild_id)
            .and_then(|guild| guild.roles.lock().insert(role.id, CachedRole::from(role)));
    }

    pub async fn get_role(
        &self,
        guild_id: Id<GuildMarker>,
        role_id: Id<RoleMarker>,
    ) -> Result<CachedRole> {
        let cached_role = {
            self.guilds
                .lock()
                .get_mut(&guild_id)
                .and_then(|guild| guild.roles.lock().get(&role_id).cloned())
        };

        match cached_role {
            Some(cached_role) => Ok(cached_role),
            None => {
                info!("role {} not in cache, fetching", role_id);

                let roles = self.http.roles(guild_id).await?.model().await?;

                for role in &roles {
                    self.put_role(guild_id, role);
                }

                let role = roles
                    .iter()
                    .find(|role| role.id == role_id)
                    .context("role does not exist")?;

                Ok(CachedRole::from(role))
            }
        }
    }

    fn put_member(
        &self,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
        member: &PartialMember,
    ) {
        self.guilds.lock().get_mut(&guild_id).and_then(|guild| {
            guild
                .members
                .lock()
                .put(user_id, CachedMember::from(member))
        });
    }

    fn put_full_member(&self, guild_id: Id<GuildMarker>, member: &Member) {
        self.put_user(&member.user);

        self.guilds.lock().get_mut(&guild_id).and_then(|guild| {
            guild
                .members
                .lock()
                .put(member.user.id, CachedMember::from(member))
        });
    }

    fn put_member_update(&self, member: &MemberUpdate) {
        self.put_user(&member.user);

        self.guilds
            .lock()
            .get_mut(&member.guild_id)
            .and_then(|guild| {
                guild
                    .members
                    .lock()
                    .put(member.user.id, CachedMember::from(member))
            });
    }

    pub async fn get_member(
        &self,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
    ) -> Result<CachedMember> {
        let cached_member = {
            self.guilds
                .lock()
                .get_mut(&guild_id)
                .and_then(|guild| guild.members.lock().get(&user_id).cloned())
        };

        match cached_member {
            Some(cached_member) => Ok(cached_member),
            None => {
                info!(
                    "member {} for guild {} not in cache, fetching",
                    user_id, guild_id
                );

                let member = self
                    .http
                    .guild_member(guild_id, user_id)
                    .await?
                    .model()
                    .await?;

                self.put_full_member(guild_id, &member);

                Ok(CachedMember::from(&member))
            }
        }
    }

    fn put_channel(&self, channel: &Channel) {
        if let Some(guild_id) = channel.guild_id {
            let mut guild = self.guilds.lock();
            if let Some(guild) = guild.get_mut(&guild_id) {
                let mut cache = guild.channels.lock();
                cache.insert(channel.id, CachedChannel::from(channel));
            }
        } else {
            warn!(?channel, "cache got a non-guild channel");
        }
    }

    pub async fn get_channel(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> Result<CachedChannel> {
        let cached_channel = {
            self.guilds
                .lock()
                .get_mut(&guild_id)
                .and_then(|guild| guild.channels.lock().get(&channel_id).cloned())
        };

        match cached_channel {
            Some(cached_channel) => Ok(cached_channel),
            None => {
                info!("channel {} not in cache, fetching", channel_id);

                let channel = self.http.channel(channel_id).await?.model().await?;

                if channel.guild_id != Some(guild_id) {
                    warn!(
                        ?channel,
                        ?guild_id,
                        "fetched channel guild id does not match expected guild"
                    );
                }

                self.put_channel(&channel);

                Ok(CachedChannel::from(&channel))
            }
        }
    }

    fn put_message(&self, message: &Message) {
        self.put_user(&message.author);

        if let (Some(guild_id), Some(member)) = (message.guild_id, &message.member) {
            self.put_member(guild_id, message.author.id, member);
        }

        for mentioned_user in &message.mentions {
            self.put_user_mention(message.guild_id, mentioned_user);
        }

        if let Some(guild_id) = message.guild_id {
            self.guilds.lock().get_mut(&guild_id).and_then(|guild| {
                guild
                    .messages
                    .lock()
                    .put(message.id, CachedMessage::from(message))
            });
        } else {
            warn!(?message, "cache got a non-guild message")
        }
    }

    fn put_message_update(&self, message: &MessageUpdate) {
        if let Some(author) = &message.author {
            self.put_user(author);
        }

        if let Some(mentions) = &message.mentions {
            for mention in mentions {
                self.put_user_mention(message.guild_id, mention);
            }
        }

        // The things we store can't change, so there is no need for this.
        // if let (Some(guild_id), Some(author), Some(kind)) =
        //     (message.guild_id, &message.author, message.kind)
        // {
        //     self.guilds.lock().get_mut(&guild_id).and_then(|guild| {
        //         guild.messages.lock().put(
        //             message.id,
        //             CachedMessage {
        //                 author_id: author.id,
        //                 kind,
        //             },
        //         )
        //     });
        // }
    }

    pub async fn get_message(
        &self,
        guild_id: Option<Id<GuildMarker>>,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
    ) -> Result<CachedMessage> {
        let cached_message = if let Some(guild_id) = guild_id {
            self.guilds
                .lock()
                .get_mut(&guild_id)
                .and_then(|guild| guild.messages.lock().get(&message_id).cloned())
        } else {
            warn!(
                ?channel_id,
                ?message_id,
                "non-guild message requested from cache"
            );

            None
        };

        match cached_message {
            Some(cached_message) => Ok(cached_message),
            None => {
                info!("message {} not in cache, fetching", message_id);

                let mut message = self
                    .http
                    .message(channel_id, message_id)
                    .await?
                    .model()
                    .await?;

                // Messages returned by the API don't have a guild id.
                message.guild_id = guild_id;

                self.put_message(&message);

                Ok(CachedMessage::from(&message))
            }
        }
    }
}

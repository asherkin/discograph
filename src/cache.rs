use anyhow::{Context, Result};
use futures::future::join_all;
use lru::LruCache;
use parking_lot::Mutex;
use tokio::sync::oneshot;
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
use std::fmt;
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
    pub roles: Vec<Id<RoleMarker>>,
}

impl From<&PartialMember> for CachedMember {
    fn from(member: &PartialMember) -> Self {
        CachedMember {
            nick: member.nick.clone(),
            roles: member.roles.clone(),
        }
    }
}

impl From<&Member> for CachedMember {
    fn from(member: &Member) -> Self {
        CachedMember {
            nick: member.nick.clone(),
            roles: member.roles.clone(),
        }
    }
}

impl From<&MemberUpdate> for CachedMember {
    fn from(member: &MemberUpdate) -> Self {
        CachedMember {
            nick: member.nick.clone(),
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

// TODO: I don't think the rest of these should be LRU other than messages, as we need them for
//       all active objects. Investigate more once we have the GraphMap implemented.
//       A bonus of non-LRU maps here would be the ability to use RwLock.
// TODO: Rewrite this to be partitioned per-guild.
#[allow(clippy::type_complexity)]
pub struct Cache {
    http: Arc<Client>,
    pending_guild_members: Mutex<HashMap<String, oneshot::Sender<Vec<Id<UserMarker>>>>>,
    users: Mutex<LruCache<Id<UserMarker>, CachedUser>>,
    guilds: Mutex<LruCache<Id<GuildMarker>, CachedGuild>>,
    roles: Mutex<LruCache<Id<RoleMarker>, CachedRole>>,
    members: Mutex<LruCache<(Id<GuildMarker>, Id<UserMarker>), CachedMember>>,
    channels: Mutex<LruCache<Id<ChannelMarker>, CachedChannel>>,
    /// Used to lookup the author of messages being reacted to.
    messages: Mutex<LruCache<Id<MessageMarker>, CachedMessage>>,
}

/// A newtype to wrap LruCache, as LruCache's Debug impl doesn't print the container contents.
struct PrintableLruCache<'a, K, V>(&'a Mutex<LruCache<K, V>>);

impl<K: std::cmp::Eq + std::hash::Hash + fmt::Debug, V: fmt::Debug> fmt::Debug
    for PrintableLruCache<'_, K, V>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut m = f.debug_map();
        for (k, v) in self.0.lock().iter() {
            // Manually use format_args! to not propagate the alternate rendering mode
            // so we get a more compat representation due to the size of these maps.
            m.entry(&format_args!("{:?}", k), &format_args!("{:?}", v));
        }
        m.finish()
    }
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Cache")
            .field("users", &PrintableLruCache(&self.users))
            .field("guilds", &PrintableLruCache(&self.guilds))
            .field("roles", &PrintableLruCache(&self.roles))
            .field("members", &PrintableLruCache(&self.members))
            .field("channels", &PrintableLruCache(&self.channels))
            .field("messages", &PrintableLruCache(&self.messages))
            .finish()
    }
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

// The `get_*` functions in here release the lock while processing in order to support async in
// the future, and a potential switch to RwLock if we move away from LruCache.
impl Cache {
    pub fn new(http: Arc<Client>) -> Self {
        // TODO: Tune these cache sizes.
        let cache_limit = NonZeroUsize::new(5000).unwrap();

        Cache {
            http,
            pending_guild_members: Mutex::new(HashMap::new()),
            users: Mutex::new(LruCache::new(cache_limit)),
            guilds: Mutex::new(LruCache::new(cache_limit)),
            roles: Mutex::new(LruCache::new(cache_limit)),
            members: Mutex::new(LruCache::new(cache_limit)),
            channels: Mutex::new(LruCache::new(cache_limit)),
            messages: Mutex::new(LruCache::new(cache_limit)),
        }
    }

    pub fn get_stats(&self) -> CacheStats {
        CacheStats {
            users: self.users.lock().len(),
            guilds: self.guilds.lock().len(),
            roles: self.roles.lock().len(),
            members: self.members.lock().len(),
            channels: self.channels.lock().len(),
            messages: self.messages.lock().len(),
        }
    }

    pub fn update(&self, event: &Event) {
        match event {
            Event::ChannelCreate(channel) => self.put_channel(channel),
            Event::ChannelUpdate(channel) => self.put_channel(channel),
            Event::ChannelDelete(channel) => {
                self.channels.lock().pop(&channel.id);
            }
            Event::GuildCreate(guild) => self.put_full_guild(guild),
            Event::GuildUpdate(guild) => self.put_guild(guild),
            Event::GuildDelete(guild) => {
                self.guilds.lock().pop(&guild.id);
            }
            Event::MemberAdd(member) => self.put_full_member(member.guild_id, member),
            Event::MemberUpdate(member) => self.put_member_update(member),
            Event::MemberChunk(chunk) => {
                for member in &chunk.members {
                    self.put_full_member(chunk.guild_id, member)
                }

                if let Some(nonce) = &chunk.nonce {
                    if let Some(sender) = self.pending_guild_members.lock().remove(nonce) {
                        if sender.send(chunk.not_found.clone()).is_err() {
                            warn!("failed to notify member chunk with nonce {}", nonce);
                        }
                    } else {
                        warn!(
                            "got member chunk with nonce {}, but there was no pending request",
                            nonce
                        );
                    }
                }
            }
            Event::MemberRemove(member) => {
                self.put_user(&member.user);
                self.members.lock().pop(&(member.guild_id, member.user.id));
            }
            Event::MessageCreate(message) => self.put_message(message),
            Event::MessageUpdate(message) => self.put_message_update(message),
            Event::MessageDelete(message) => {
                self.messages.lock().pop(&message.id);
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
            Event::RoleCreate(role) => self.put_role(&role.role),
            Event::RoleUpdate(role) => self.put_role(&role.role),
            Event::RoleDelete(role) => {
                self.roles.lock().pop(&role.role_id);
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

        debug!("cache stats: {:?}", self.get_stats());
    }

    /// Bulk request member info for a guild via the gateway.
    ///
    /// Returns the IDs of members that could not be loaded by Discord.
    pub async fn bulk_preload_members(
        &self,
        shard: &MessageSender,
        guild_id: Id<GuildMarker>,
        user_ids: impl Iterator<Item = Id<UserMarker>>,
    ) -> Result<Vec<Id<UserMarker>>> {
        let users_to_load: Vec<_> = {
            let members = self.members.lock();
            user_ids
                .filter(|user_id| !members.contains(&(guild_id, *user_id)))
                .collect()
        };

        if users_to_load.is_empty() {
            return Ok(Vec::new());
        }

        let (nonces, commands): (Vec<_>, Vec<_>) = users_to_load
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
            .unzip();

        info!(
            "requesting {} members for guild {} in {} chunks",
            users_to_load.len(),
            guild_id,
            nonces.len(),
        );

        let futures: Vec<_> = {
            let mut pending_guild_members = self.pending_guild_members.lock();

            nonces
                .iter()
                .zip(commands.into_iter())
                .map(|(nonce, command)| {
                    let (sender, receiver) = oneshot::channel();
                    if pending_guild_members
                        .insert(nonce.clone(), sender)
                        .is_some()
                    {
                        panic!("guild member chunk request nonce collision occurred");
                    }

                    if let Err(error) = shard.command(&command) {
                        warn!(?command, ?error, "failed to request member chunk");
                    }

                    receiver
                })
                .collect()
        };

        let future = timeout(
            Duration::from_secs(5 * (nonces.len() as u64)),
            join_all(futures),
        );

        let results = match future.await {
            Ok(results) => results,
            Err(elapsed) => {
                warn!(
                    "member chunk request for guild {} timed out after {}",
                    guild_id, elapsed
                );

                let mut pending_guild_members = self.pending_guild_members.lock();

                for nonce in nonces {
                    pending_guild_members.remove(&nonce);
                }

                return Ok(Vec::new());
            }
        };

        let not_found: Vec<_> = results.into_iter().flatten().flatten().collect();

        Ok(not_found)
    }

    fn put_user(&self, user: &User) {
        let mut cache = self.users.lock();
        cache.put(user.id, CachedUser::from(user));
    }

    fn put_user_mention(&self, mention: &Mention) {
        let mut cache = self.users.lock();
        cache.put(mention.id, CachedUser::from(mention));
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
        for role in &guild.roles {
            self.put_role(role);
        }

        let mut cache = self.guilds.lock();
        cache.put(guild.id, CachedGuild::from(guild));
    }

    fn put_full_guild(&self, guild: &Guild) {
        for channel in &guild.channels {
            self.put_channel(channel);
        }

        for role in &guild.roles {
            self.put_role(role);
        }

        let mut cache = self.guilds.lock();
        cache.put(guild.id, CachedGuild::from(guild));
    }

    pub async fn get_guild(&self, guild_id: Id<GuildMarker>) -> Result<CachedGuild> {
        let cached_guild = {
            let mut cache = self.guilds.lock();
            cache.get(&guild_id).cloned()
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

    fn put_role(&self, role: &Role) {
        let mut cache = self.roles.lock();
        cache.put(role.id, CachedRole::from(role));
    }

    pub async fn get_role(
        &self,
        guild_id: Id<GuildMarker>,
        role_id: Id<RoleMarker>,
    ) -> Result<CachedRole> {
        let cached_role = {
            let mut cache = self.roles.lock();
            cache.get(&role_id).cloned()
        };

        match cached_role {
            Some(cached_role) => Ok(cached_role),
            None => {
                info!("role {} not in cache, fetching", role_id);

                let roles = self.http.roles(guild_id).await?.model().await?;

                for role in &roles {
                    self.put_role(role);
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
        let mut cache = self.members.lock();
        cache.put((guild_id, user_id), CachedMember::from(member));
    }

    fn put_full_member(&self, guild_id: Id<GuildMarker>, member: &Member) {
        self.put_user(&member.user);

        let mut cache = self.members.lock();
        cache.put((guild_id, member.user.id), CachedMember::from(member));
    }

    fn put_member_update(&self, member: &MemberUpdate) {
        self.put_user(&member.user);

        let mut cache = self.members.lock();
        cache.put(
            (member.guild_id, member.user.id),
            CachedMember::from(member),
        );
    }

    pub async fn get_member(
        &self,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
    ) -> Result<CachedMember> {
        let cached_member = {
            let mut cache = self.members.lock();
            cache.get(&(guild_id, user_id)).cloned()
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
        let mut cache = self.channels.lock();
        cache.put(channel.id, CachedChannel::from(channel));
    }

    pub async fn get_channel(&self, channel_id: Id<ChannelMarker>) -> Result<CachedChannel> {
        let cached_channel = {
            let mut cache = self.channels.lock();
            cache.get(&channel_id).cloned()
        };

        match cached_channel {
            Some(cached_channel) => Ok(cached_channel),
            None => {
                info!("channel {} not in cache, fetching", channel_id);

                let channel = self.http.channel(channel_id).await?.model().await?;

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
            self.put_user_mention(mentioned_user);

            // We can't do this in `put_user_mention` as it needs the guild ID.
            if let (Some(guild_id), Some(member)) = (message.guild_id, &mentioned_user.member) {
                self.put_member(guild_id, mentioned_user.id, member);
            }
        }

        let mut cache = self.messages.lock();
        cache.put(message.id, CachedMessage::from(message));
    }

    fn put_message_update(&self, message: &MessageUpdate) {
        if let Some(author) = &message.author {
            self.put_user(author);
        }

        if let Some(mentions) = &message.mentions {
            for mention in mentions {
                self.put_user_mention(mention);

                // We can't do this in `put_user_mention` as it needs the guild ID.
                if let (Some(guild_id), Some(member)) = (message.guild_id, &mention.member) {
                    self.put_member(guild_id, mention.id, member);
                }
            }
        }

        if let (Some(author), Some(kind)) = (&message.author, message.kind) {
            let mut cache = self.messages.lock();
            cache.put(
                message.id,
                CachedMessage {
                    author_id: author.id,
                    kind,
                },
            );
        }
    }

    pub async fn get_message(
        &self,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
    ) -> Result<CachedMessage> {
        let cached_message = {
            let mut cache = self.messages.lock();
            cache.get(&message_id).cloned()
        };

        match cached_message {
            Some(cached_message) => Ok(cached_message),
            None => {
                info!("message {} not in cache, fetching", message_id);

                let message = self
                    .http
                    .message(channel_id, message_id)
                    .await?
                    .model()
                    .await?;

                self.put_message(&message);

                Ok(CachedMessage::from(&message))
            }
        }
    }
}

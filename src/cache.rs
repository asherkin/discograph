use anyhow::{Context, Result};
use lru::LruCache;
use parking_lot::Mutex;
use tracing::info;
use twilight_http::Client;
use twilight_model::channel::message::MessageType;
use twilight_model::channel::permission_overwrite::PermissionOverwrite;
use twilight_model::channel::{Channel, ChannelType, GuildChannel, Message, TextChannel};
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::MemberUpdate;
use twilight_model::guild::{Guild, Member, PartialGuild, PartialMember, Permissions, Role};
use twilight_model::id::{ChannelId, GuildId, MessageId, RoleId, UserId};
use twilight_model::user::User;

use std::fmt;

#[derive(Debug, Clone)]
pub struct CachedUser {
    pub id: UserId,
    pub name: String,
    pub discriminator: u16,
    pub avatar: Option<String>,
    pub bot: bool,
}

impl From<&User> for CachedUser {
    fn from(user: &User) -> Self {
        CachedUser {
            id: user.id,
            name: user.name.clone(),
            // TODO: Decide if we want to switch to storing a String here
            discriminator: user.discriminator.parse().unwrap(),
            avatar: user.avatar.clone(),
            bot: user.bot,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedGuild {
    pub id: GuildId,
    pub name: String,
    pub icon: Option<String>,
    pub roles: Vec<RoleId>,
    pub owner_id: UserId,
}

impl From<&PartialGuild> for CachedGuild {
    fn from(guild: &PartialGuild) -> Self {
        CachedGuild {
            id: guild.id,
            name: guild.name.clone(),
            icon: guild.icon.clone(),
            roles: guild.roles.keys().cloned().collect(),
            owner_id: guild.owner_id,
        }
    }
}

impl From<&Guild> for CachedGuild {
    fn from(guild: &Guild) -> Self {
        CachedGuild {
            id: guild.id,
            name: guild.name.clone(),
            icon: guild.icon.clone(),
            roles: guild.roles.keys().cloned().collect(),
            owner_id: guild.owner_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedRole {
    pub id: RoleId,
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
    pub roles: Vec<RoleId>,
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
    pub id: ChannelId,
    pub name: String,
    pub kind: ChannelType,
    pub permission_overwrites: Vec<PermissionOverwrite>,
}

impl From<&TextChannel> for CachedChannel {
    fn from(channel: &TextChannel) -> Self {
        CachedChannel {
            id: channel.id,
            name: channel.name.clone(),
            kind: channel.kind,
            permission_overwrites: channel.permission_overwrites.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedMessage {
    pub author_id: UserId,
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
pub struct Cache {
    http: Client,
    users: Mutex<LruCache<UserId, CachedUser>>,
    guilds: Mutex<LruCache<GuildId, CachedGuild>>,
    roles: Mutex<LruCache<RoleId, CachedRole>>,
    members: Mutex<LruCache<(GuildId, UserId), CachedMember>>,
    channels: Mutex<LruCache<ChannelId, CachedChannel>>,
    /// Used to lookup the author of messages being reacted to.
    messages: Mutex<LruCache<MessageId, CachedMessage>>,
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
    pub fn new(http: Client) -> Self {
        // TODO: Tune these cache sizes.
        const CACHE_LIMIT: usize = 5000;

        Cache {
            http,
            users: Mutex::new(LruCache::new(CACHE_LIMIT)),
            guilds: Mutex::new(LruCache::new(CACHE_LIMIT)),
            roles: Mutex::new(LruCache::new(CACHE_LIMIT)),
            members: Mutex::new(LruCache::new(CACHE_LIMIT)),
            channels: Mutex::new(LruCache::new(CACHE_LIMIT)),
            messages: Mutex::new(LruCache::new(CACHE_LIMIT)),
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
            Event::ChannelCreate(channel) => {
                if let Channel::Guild(GuildChannel::Text(channel)) = &channel.0 {
                    self.put_channel(&channel)
                }
            }
            Event::ChannelUpdate(channel) => {
                if let Channel::Guild(GuildChannel::Text(channel)) = &channel.0 {
                    self.put_channel(&channel)
                }
            }
            Event::GuildCreate(guild) => self.put_full_guild(&guild),
            Event::GuildUpdate(guild) => self.put_guild(&guild),
            Event::MemberAdd(member) => self.put_full_member(&member),
            Event::MemberUpdate(member) => self.put_member_update(&member),
            Event::MemberChunk(chunk) => {
                for member in chunk.members.values() {
                    self.put_full_member(member)
                }
            }
            Event::MessageCreate(message) => self.put_message(&message),
            // TODO: Event::MessageUpdate(message) => self.put_message(&message),
            Event::ReactionAdd(reaction) => {
                if let Some(member) = &reaction.member {
                    self.put_full_member(member);
                }
            }
            Event::RoleCreate(role) => self.put_role(&role.role),
            Event::RoleUpdate(role) => self.put_role(&role.role),
            _ => info!("event not used by cache: {:?}", event.kind()),
        }

        info!("cache stats: {:?}", self.get_stats());
    }

    fn put_user(&self, user: &User) {
        let mut cache = self.users.lock();
        cache.put(user.id, CachedUser::from(user));
    }

    pub async fn get_user(&self, user_id: UserId) -> Result<CachedUser> {
        let cached_user = {
            let mut cache = self.users.lock();
            cache.get(&user_id).cloned()
        };

        match cached_user {
            Some(cached_user) => Ok(cached_user),
            None => {
                info!("user {} not in cache, fetching", user_id);

                let user = self
                    .http
                    .user(user_id)
                    .await?
                    .context("user does not exist")?;

                self.put_user(&user);

                Ok(CachedUser::from(&user))
            }
        }
    }

    fn put_guild(&self, guild: &PartialGuild) {
        for (_, role) in guild.roles.iter() {
            self.put_role(role);
        }

        let mut cache = self.guilds.lock();
        cache.put(guild.id, CachedGuild::from(guild));
    }

    fn put_full_guild(&self, guild: &Guild) {
        for (_, channel) in guild.channels.iter() {
            if let GuildChannel::Text(channel) = channel {
                if channel.kind == ChannelType::GuildText {
                    self.put_channel(&channel);
                }
            }
        }

        for (_, role) in guild.roles.iter() {
            self.put_role(role);
        }

        let mut cache = self.guilds.lock();
        cache.put(guild.id, CachedGuild::from(guild));
    }

    pub async fn get_guild(&self, guild_id: GuildId) -> Result<CachedGuild> {
        let cached_guild = {
            let mut cache = self.guilds.lock();
            cache.get(&guild_id).cloned()
        };

        match cached_guild {
            Some(cached_guild) => Ok(cached_guild),
            None => {
                info!("guild {} not in cache, fetching", guild_id);

                let guild = self
                    .http
                    .guild(guild_id)
                    .await?
                    .context("guild does not exist")?;

                self.put_full_guild(&guild);

                Ok(CachedGuild::from(&guild))
            }
        }
    }

    fn put_role(&self, role: &Role) {
        let mut cache = self.roles.lock();
        cache.put(role.id, CachedRole::from(role));
    }

    pub async fn get_role(&self, guild_id: GuildId, role_id: RoleId) -> Result<CachedRole> {
        let cached_role = {
            let mut cache = self.roles.lock();
            cache.get(&role_id).cloned()
        };

        match cached_role {
            Some(cached_role) => Ok(cached_role),
            None => {
                info!("role {} not in cache, fetching", role_id);

                let roles = self.http.roles(guild_id).await?;

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

    fn put_member(&self, guild_id: GuildId, user_id: UserId, member: &PartialMember) {
        // TODO: https://github.com/twilight-rs/twilight/issues/566
        // if let Some(user) = &member.user {
        //     let user = user.read();
        //     self.put_user(&*user);
        // }

        let mut cache = self.members.lock();
        cache.put((guild_id, user_id), CachedMember::from(member));
    }

    fn put_full_member(&self, member: &Member) {
        self.put_user(&member.user);

        let mut cache = self.members.lock();
        cache.put(
            (member.guild_id, member.user.id),
            CachedMember::from(member),
        );
    }

    fn put_member_update(&self, member: &MemberUpdate) {
        self.put_user(&member.user);

        let mut cache = self.members.lock();
        cache.put(
            (member.guild_id, member.user.id),
            CachedMember::from(member),
        );
    }

    pub async fn get_member(&self, guild_id: GuildId, user_id: UserId) -> Result<CachedMember> {
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
                    .context("member does not exist")?;

                self.put_full_member(&member);

                Ok(CachedMember::from(&member))
            }
        }
    }

    fn put_channel(&self, channel: &TextChannel) {
        let mut cache = self.channels.lock();
        cache.put(channel.id, CachedChannel::from(channel));
    }

    pub async fn get_channel(&self, channel_id: ChannelId) -> Result<CachedChannel> {
        let cached_channel = {
            let mut cache = self.channels.lock();
            cache.get(&channel_id).cloned()
        };

        match cached_channel {
            Some(cached_channel) => Ok(cached_channel),
            None => {
                info!("channel {} not in cache, fetching", channel_id);

                let channel = self
                    .http
                    .channel(channel_id)
                    .await?
                    .context("channel does not exist")?;

                match channel {
                    Channel::Guild(GuildChannel::Text(channel)) => {
                        self.put_channel(&channel);

                        Ok(CachedChannel::from(&channel))
                    }
                    _ => anyhow::bail!("not a guild text channel"),
                }
            }
        }
    }

    fn put_message(&self, message: &Message) {
        self.put_user(&message.author);

        if let (Some(guild_id), Some(member)) = (message.guild_id, &message.member) {
            self.put_member(guild_id, message.author.id, member);
        }

        for mentioned_user in message.mentions.values() {
            self.put_user(mentioned_user);

            // We can't do this in `put_user` as it needs the guild ID.
            // TODO: https://github.com/twilight-rs/twilight/issues/566
            // if let (Some(guild_id), Some(member)) = (message.guild_id, &mentioned_user.member) {
            //     self.put_member(guild_id, mentioned_user.id, member);
            // }
        }

        let mut cache = self.messages.lock();
        cache.put(message.id, CachedMessage::from(message));
    }

    pub async fn get_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
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
                    .context("message does not exist")?;

                self.put_message(&message);

                Ok(CachedMessage::from(&message))
            }
        }
    }
}

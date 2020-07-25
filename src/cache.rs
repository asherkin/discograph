use lru::LruCache;
use serenity::http::Http;
use serenity::model::prelude::*;
use serenity::prelude::{Mutex, SerenityError};
use serenity::utils::Color;
use serenity::Result as SerenityResult;

#[derive(Clone)]
pub(crate) struct CachedUser {
    pub id: UserId,
    pub name: String,
    pub discriminator: u16,
}

impl From<&User> for CachedUser {
    fn from(user: &User) -> Self {
        CachedUser {
            id: user.id,
            name: user.name.clone(),
            discriminator: user.discriminator,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CachedGuild {
    pub name: String,
}

impl From<&PartialGuild> for CachedGuild {
    fn from(guild: &PartialGuild) -> Self {
        CachedGuild {
            name: guild.name.clone(),
        }
    }
}

impl From<&Guild> for CachedGuild {
    fn from(guild: &Guild) -> Self {
        CachedGuild {
            name: guild.name.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CachedRole {
    pub name: String,
    pub color: Color,
    pub position: i64,
}

impl From<&Role> for CachedRole {
    fn from(role: &Role) -> Self {
        CachedRole {
            name: role.name.clone(),
            color: role.colour,
            position: role.position,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CachedMember {
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

#[derive(Clone)]
pub(crate) struct CachedChannel {
    pub name: String,
}

impl From<&GuildChannel> for CachedChannel {
    fn from(channel: &GuildChannel) -> Self {
        CachedChannel {
            name: channel.name.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CachedMessage {
    pub author_id: UserId,
}

impl From<&Message> for CachedMessage {
    fn from(message: &Message) -> Self {
        CachedMessage {
            author_id: message.author.id,
        }
    }
}

/// We keep our own cache due as serenity does not backfill its cache on requests.
// TODO: I don't think the rest of these should be LRU other than messages, as we need them for
//       all active objects. Investigate more once we have the GraphMap implemented.
//       A bonus of non-LRU maps here would be the ability to use RwLock.
pub(crate) struct Cache {
    users: Mutex<LruCache<UserId, CachedUser>>,
    guilds: Mutex<LruCache<GuildId, CachedGuild>>,
    roles: Mutex<LruCache<RoleId, CachedRole>>,
    members: Mutex<LruCache<(GuildId, UserId), CachedMember>>,
    channels: Mutex<LruCache<ChannelId, CachedChannel>>,
    /// Used to lookup the author of messages being reacted to.
    messages: Mutex<LruCache<MessageId, CachedMessage>>,
}

/// The `get_*` functions in here release the lock while processing in order to support async in
/// the future, and a potential switch to RwLock.
impl Cache {
    pub fn new() -> Self {
        // TODO: Tune these cache sizes.
        const CACHE_LIMIT: usize = 1000;

        Cache {
            users: Mutex::new(LruCache::new(CACHE_LIMIT)),
            guilds: Mutex::new(LruCache::new(CACHE_LIMIT)),
            roles: Mutex::new(LruCache::new(CACHE_LIMIT)),
            members: Mutex::new(LruCache::new(CACHE_LIMIT)),
            channels: Mutex::new(LruCache::new(CACHE_LIMIT)),
            messages: Mutex::new(LruCache::new(CACHE_LIMIT)),
        }
    }

    pub fn put_user(&self, user: &User) {
        let mut cache = self.users.lock();
        cache.put(user.id, CachedUser::from(user));
    }

    pub fn get_user(&self, http: impl AsRef<Http>, user_id: UserId) -> SerenityResult<CachedUser> {
        let cached_user = {
            let mut cache = self.users.lock();
            cache.get(&user_id).cloned()
        };

        match cached_user {
            Some(cached_user) => Ok(cached_user),
            None => http.as_ref().get_user(user_id.into()).map(|user| {
                self.put_user(&user);

                CachedUser::from(&user)
            }),
        }
    }

    pub fn put_guild(&self, guild: &PartialGuild) {
        let mut cache = self.guilds.lock();
        cache.put(guild.id, CachedGuild::from(guild));
    }

    pub fn put_full_guild(&self, guild: &Guild) {
        for (_, channel) in guild.channels.iter() {
            self.put_channel(&*channel.read());
        }

        for (_, role) in guild.roles.iter() {
            self.put_role(role);
        }

        let mut cache = self.guilds.lock();
        cache.put(guild.id, CachedGuild::from(guild));
    }

    pub fn get_guild(
        &self,
        http: impl AsRef<Http>,
        guild_id: GuildId,
    ) -> SerenityResult<CachedGuild> {
        let cached_guild = {
            let mut cache = self.guilds.lock();
            cache.get(&guild_id).cloned()
        };

        match cached_guild {
            Some(cached_guild) => Ok(cached_guild),
            None => http.as_ref().get_guild(guild_id.into()).map(|guild| {
                self.put_guild(&guild);

                CachedGuild::from(&guild)
            }),
        }
    }

    pub fn put_role(&self, role: &Role) {
        let mut cache = self.roles.lock();
        cache.put(role.id, CachedRole::from(role));
    }

    pub fn get_role(
        &self,
        http: impl AsRef<Http>,
        guild_id: GuildId,
        role_id: RoleId,
    ) -> SerenityResult<CachedRole> {
        let cached_role = {
            let mut cache = self.roles.lock();
            cache.get(&role_id).cloned()
        };

        match cached_role {
            Some(cached_role) => Ok(cached_role),
            None => http
                .as_ref()
                .get_guild_roles(guild_id.into())
                .map(|roles| {
                    for role in &roles {
                        self.put_role(role);
                    }

                    // TODO: Re-using serenity's errors here is probably not sane.
                    roles
                        .iter()
                        .find(|role| role.id == role_id)
                        .map(|role| CachedRole::from(role))
                        .ok_or(SerenityError::Model(
                            serenity::model::error::Error::RoleNotFound,
                        ))
                })?,
        }
    }

    pub fn put_member(&self, guild_id: GuildId, user_id: UserId, member: &PartialMember) {
        if let Some(user) = &member.user {
            self.put_user(user);
        }

        let mut cache = self.members.lock();
        cache.put((guild_id, user_id), CachedMember::from(member));
    }

    pub fn put_full_member(&self, member: &Member) {
        let user = member.user.read();

        self.put_user(&*user);

        let mut cache = self.members.lock();
        cache.put((member.guild_id, user.id), CachedMember::from(member));
    }

    pub fn get_member(
        &self,
        http: impl AsRef<Http>,
        guild_id: GuildId,
        user_id: UserId,
    ) -> SerenityResult<CachedMember> {
        let cached_member = {
            let mut cache = self.members.lock();
            cache.get(&(guild_id, user_id)).cloned()
        };

        match cached_member {
            Some(cached_member) => Ok(cached_member),
            None => http
                .as_ref()
                .get_member(guild_id.into(), user_id.into())
                .map(|member| {
                    self.put_full_member(&member);

                    CachedMember::from(&member)
                }),
        }
    }

    pub fn put_channel(&self, channel: &GuildChannel) {
        let mut cache = self.channels.lock();
        cache.put(channel.id, CachedChannel::from(channel));
    }

    pub fn get_channel(
        &self,
        http: impl AsRef<Http>,
        channel_id: ChannelId,
    ) -> SerenityResult<CachedChannel> {
        let cached_channel = {
            let mut cache = self.channels.lock();
            cache.get(&channel_id).cloned()
        };

        match cached_channel {
            Some(cached_channel) => Ok(cached_channel),
            None => http.as_ref().get_channel(channel_id.into()).map(|channel| {
                let channel = channel.guild().expect("not a guild channel");
                let channel = channel.read();

                self.put_channel(&*channel);

                CachedChannel::from(&*channel)
            }),
        }
    }

    pub fn put_message(&self, message: &Message) {
        self.put_user(&message.author);

        if let (Some(guild_id), Some(member)) = (message.guild_id, &message.member) {
            self.put_member(guild_id, message.author.id, &member);
        }

        let mut cache = self.messages.lock();
        cache.put(message.id, CachedMessage::from(message));
    }

    pub fn get_message(
        &self,
        http: impl AsRef<Http>,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> SerenityResult<CachedMessage> {
        let cached_message = {
            let mut cache = self.messages.lock();
            cache.get(&message_id).cloned()
        };

        match cached_message {
            Some(cached_message) => Ok(cached_message),
            None => http
                .as_ref()
                .get_message(channel_id.into(), message_id.into())
                .map(|message| {
                    self.put_message(&message);

                    CachedMessage::from(&message)
                }),
        }
    }
}

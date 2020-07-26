use serenity::model::prelude::*;
use serenity::prelude::{Context, EventHandler, RwLock};

use std::sync::Arc;

use crate::cache::Cache;
use crate::inference::Interaction;
use crate::parsing::Command;
use serenity::utils::Color;

pub(crate) struct Handler {
    id: RwLock<Option<UserId>>,
    cache: Cache,
}

impl Handler {
    pub fn new() -> Self {
        Handler {
            id: RwLock::new(None),
            cache: Cache::new(),
        }
    }
}

// noinspection RsSortImplTraitMembers
impl EventHandler for Handler {
    fn ready(&self, ctx: Context, data: Ready) {
        self.id.write().replace(data.user.id);

        ctx.set_activity(Activity::watching(&format!(
            "you: @{} invite",
            data.user.name
        )));
    }

    fn guild_create(&self, _ctx: Context, guild: Guild) {
        self.cache.put_full_guild(&guild);
    }

    fn guild_update(&self, _ctx: Context, guild: PartialGuild) {
        self.cache.put_guild(&guild);
    }

    fn guild_role_create(&self, _ctx: Context, _guild_id: GuildId, role: Role) {
        self.cache.put_role(&role);
    }

    fn guild_role_update(&self, _ctx: Context, _guild_id: GuildId, role: Role) {
        self.cache.put_role(&role);
    }

    fn channel_create(&self, _ctx: Context, channel: Arc<RwLock<GuildChannel>>) {
        let channel = channel.read();
        if channel.kind == ChannelType::Text {
            self.cache.put_channel(&channel);
        }
    }

    fn channel_update(&self, _ctx: Context, channel: Channel) {
        if let Some(channel) = channel.guild() {
            let channel = channel.read();
            if channel.kind == ChannelType::Text {
                self.cache.put_channel(&channel);
            }
        }
    }

    fn message(&self, ctx: Context, new_message: Message) {
        if new_message.guild_id.is_none() || new_message.member.is_none() {
            return;
        }

        let our_id = self.id.read().unwrap();
        if new_message.author.id == our_id {
            return;
        }

        if new_message.mentions_user_id(our_id) {
            if let Some(command) = Command::new_from_message(our_id, &new_message.content) {
                match command {
                    Command::Invite => {
                        new_message.channel_id.send_message(&ctx, |message| {
                            message.embed(|embed| {
                                embed.title("Invite me!")
                                    .url(format!("https://discord.com/api/oauth2/authorize?client_id={}&permissions=85056&scope=bot", our_id))
                                    .description(format!("Click the link above to invite me to your server\n\nRequested By: {}#{:04}", new_message.author.name, new_message.author.discriminator))
                                    .color(Color::from_rgb(255, 255, 255))
                                    .thumbnail(/* TODO: Host our own copy. */ "https://i.imgur.com/CZFt69d.png")
                            })
                        }).unwrap();
                    }
                    Command::CacheStats => {
                        new_message
                            .reply(&ctx, format!("{:?}", self.cache.get_stats()))
                            .unwrap();
                    }
                    Command::CacheDump => {
                        println!("{:#?}", self.cache);
                        new_message.react(&ctx, "\u{2705}").unwrap();
                    }
                    Command::Unknown(command) => {
                        new_message
                            .reply(&ctx, format!("Unknown command: {}", command))
                            .unwrap();
                    }
                };

                return;
            }
        }

        self.cache.put_message(&new_message);

        let interaction = Interaction::new_from_message(new_message);
        println!("{}", interaction.to_string(&ctx, &self.cache));
    }

    fn reaction_add(&self, ctx: Context, add_reaction: Reaction) {
        if add_reaction.guild_id.is_none() || add_reaction.member.is_none() {
            return;
        }

        let our_id = self.id.read().unwrap();
        if add_reaction.user_id == our_id {
            return;
        }

        self.cache.put_member(
            add_reaction.guild_id.unwrap(),
            add_reaction.user_id,
            add_reaction.member.as_ref().unwrap(),
        );

        let message_info = self
            .cache
            .get_message(&ctx, add_reaction.channel_id, add_reaction.message_id)
            .unwrap();

        let interaction = Interaction::new_from_reaction(add_reaction, message_info);
        println!("{}", interaction.to_string(&ctx, &self.cache));
    }
}

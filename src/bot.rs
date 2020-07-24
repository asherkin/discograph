use serenity::model::prelude::*;
use serenity::prelude::{Context, EventHandler, RwLock};

use std::sync::Arc;

use crate::cache::Cache;
use crate::inference::Interaction;

pub(crate) struct Handler {
    cache: Cache,
}

impl Handler {
    pub fn new() -> Self {
        Handler {
            cache: Cache::new(),
        }
    }
}

// noinspection RsSortImplTraitMembers
impl EventHandler for Handler {
    fn guild_create(&self, _ctx: Context, guild: Guild) {
        self.cache.put_full_guild(&guild);
    }

    fn guild_update(&self, _ctx: Context, guild: PartialGuild) {
        self.cache.put_guild(&guild);
    }

    fn channel_create(&self, _ctx: Context, channel: Arc<RwLock<GuildChannel>>) {
        let channel = channel.read();
        self.cache.put_channel(&channel);
    }

    fn channel_update(&self, _ctx: Context, channel: Channel) {
        if let Some(channel) = channel.guild() {
            let channel = channel.read();
            self.cache.put_channel(&channel);
        }
    }

    fn message(&self, ctx: Context, new_message: Message) {
        if new_message.is_private() {
            // if !new_message.is_own(&ctx) {
            //     new_message.reply(&ctx, "Please don't DM me :(")
            //         .expect("failed to send");
            // }

            return;
        }

        self.cache.put_message(&new_message);

        let interaction = Interaction::new_from_message(new_message);
        println!("{}", interaction.into_friendly_string(&ctx, &self.cache));
    }

    fn reaction_add(&self, ctx: Context, add_reaction: Reaction) {
        if add_reaction.guild_id.is_none() {
            // Ignore reactions to DM'd messaged.
            return;
        }

        let message_info = self
            .cache
            .get_message(&ctx, add_reaction.channel_id, add_reaction.message_id)
            .unwrap();

        let interaction = Interaction::new_from_reaction(add_reaction, message_info);
        println!("{}", interaction.into_friendly_string(&ctx, &self.cache));
    }
}

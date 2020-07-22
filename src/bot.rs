use lru::LruCache;
use serenity::model::channel::{Message, Reaction};
use serenity::model::id::{ChannelId, MessageId, UserId};
use serenity::prelude::{Context, EventHandler};

use crate::inference::Interaction;

use std::sync::Mutex;

#[derive(Copy, Clone)]
pub(crate) struct CachedMessageInfo {
    pub author_id: UserId,
    pub channel_id: ChannelId,
}

impl From<&Message> for CachedMessageInfo {
    fn from(message: &Message) -> Self {
        CachedMessageInfo {
            author_id: message.author.id,
            channel_id: message.channel_id,
        }
    }
}

/// We keep our own message cache as serenity's message cache seems a bit limited and difficult
/// to use. TODO: We might want to have all our own caches and turn off the cache feature.
pub(crate) struct Handler {
    message_info_cache: Mutex<LruCache<MessageId, CachedMessageInfo>>,
}

impl Handler {
    pub fn new() -> Self {
        Handler {
            message_info_cache: Mutex::new(LruCache::new(1000)),
        }
    }
}

impl EventHandler for Handler {
    fn message(&self, _ctx: Context, new_message: Message) {
        if new_message.is_private() {
            // if !new_message.is_own(&ctx) {
            //     new_message.reply(&ctx, "Please don't DM me :(")
            //         .expect("failed to send");
            // }

            return;
        }

        {
            let mut message_info_cache = self.message_info_cache.lock().unwrap();

            message_info_cache.put(new_message.id, CachedMessageInfo::from(&new_message));
        }

        // let user_mentions = new_message
        //     .mentions
        //     .iter()
        //     .map(|u| u.id)
        //     .collect::<Vec<UserId>>();
        //
        // println!(
        //     "new message from {:?} in {:?}, mentions: {:?}",
        //     new_message.author.id, new_message.channel_id, user_mentions,
        // );

        let interaction = Interaction::new_from_message(new_message);
        println!("{:?}", interaction);
    }

    fn reaction_add(&self, ctx: Context, add_reaction: Reaction) {
        if add_reaction.guild_id.is_none() {
            // Ignore reactions to DM'd messaged.
            return;
        }

        let message_info = {
            let mut message_info_cache = self.message_info_cache.lock().unwrap();

            message_info_cache
                .get(&add_reaction.message_id)
                .cloned()
                .unwrap_or_else(|| {
                    let message = add_reaction
                        .message(&ctx)
                        .expect("message being reacted to doesn't exist");

                    let message_info = CachedMessageInfo::from(&message);

                    message_info_cache.put(message.id, message_info);

                    message_info
                })
        };

        // println!(
        //     "{:?} reacted to a message by {:?} in {:?}",
        //     add_reaction.user_id, message_info.author_id, message_info.channel_id,
        // );

        let interaction = Interaction::new_from_reaction(add_reaction, message_info);
        println!("{:?}", interaction);
    }
}

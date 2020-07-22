use serenity::model::id::{ChannelId, UserId, GuildId};

use crate::bot::CachedMessageInfo;
use serenity::model::channel::{Message, Reaction};

#[derive(Debug)]
enum InteractionType {
    Message,
    Reaction,
}

#[derive(Debug)]
pub(crate) struct Interaction {
    what: InteractionType,
    source: UserId,
    targets: Vec<UserId>,
    channel: ChannelId,
    guild: GuildId,
}

impl Interaction {
    pub fn new_from_message(message: Message) -> Self {
        let user_mentions = message
            .mentions
            .iter()
            .map(|u| u.id)
            .collect::<Vec<UserId>>();

        Interaction {
            what: InteractionType::Message,
            source: message.author.id,
            targets: user_mentions,
            channel: message.channel_id,
            guild: message.guild_id.unwrap(),
        }
    }

    pub fn new_from_reaction(reaction: Reaction, target_message: CachedMessageInfo) -> Self {
        Interaction {
            what: InteractionType::Reaction,
            source: reaction.user_id,
            targets: vec![target_message.author_id],
            channel: target_message.channel_id,
            guild: reaction.guild_id.unwrap(),
        }
    }
}

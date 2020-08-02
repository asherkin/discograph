use anyhow::{Context as ErrorContext, Result};
use serenity::client::Context;
use serenity::model::prelude::*;

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use crate::cache::{Cache, CachedMessage};
use crate::parsing::parse_direct_mention;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum InteractionType {
    Message,
    Reaction,
}

#[derive(Debug, Clone)]
pub struct Interaction {
    pub what: InteractionType,
    pub when: Instant,
    pub guild: GuildId,
    pub channel: ChannelId,
    pub source: UserId,
    pub target: Option<UserId>,
    pub other_targets: Vec<UserId>,
}

impl Interaction {
    pub fn new_from_message(message: &Message) -> Result<Self> {
        let guild_id = message
            .guild_id
            .context("tried to create an interaction from a message not sent to a guild")?;

        let direct_mention = parse_direct_mention(&message.content);

        let user_mentions = message
            .mentions
            .iter()
            .map(|u| u.id)
            .filter(|&u| Some(u) != direct_mention)
            .collect::<Vec<UserId>>();

        Ok(Interaction {
            what: InteractionType::Message,
            when: Instant::now(),
            guild: guild_id,
            channel: message.channel_id,
            source: message.author.id,
            target: direct_mention,
            other_targets: user_mentions,
        })
    }

    pub fn new_from_reaction(reaction: &Reaction, target_message: &CachedMessage) -> Result<Self> {
        let guild_id = reaction
            .guild_id
            .context("tried to create an interaction from a reaction not sent to a guild")?;

        Ok(Interaction {
            what: InteractionType::Reaction,
            when: Instant::now(),
            guild: guild_id,
            channel: reaction.channel_id,
            source: reaction.user_id,
            target: Some(target_message.author_id),
            other_targets: Vec::new(),
        })
    }

    pub fn to_string(&self, ctx: &Context, cache: &Cache) -> String {
        let get_user_display_name = |user_id: UserId| {
            cache
                .get_user(&ctx, user_id)
                .map(|user| {
                    format!(
                        "\"{}\" ({}#{:04})",
                        cache
                            .get_member(ctx, self.guild, user_id)
                            .ok()
                            .and_then(|member| member.nick)
                            .unwrap_or_else(|| user.name.clone()),
                        user.name,
                        user.discriminator,
                    )
                })
                .unwrap_or_else(|_| format!("<unknown user {}>", user_id))
        };

        let source_name = get_user_display_name(self.source);

        let target_names = (self.target.iter())
            .chain(self.other_targets.iter())
            .map(|&user_id| get_user_display_name(user_id))
            .collect::<Vec<String>>()
            .join(", ");

        let channel_name = cache
            .get_channel(&ctx, self.channel)
            .map(|channel| format!("#{}", channel.name))
            .unwrap_or_else(|_| format!("<unknown channel {}>", self.channel));

        let guild_name = cache
            .get_guild(&ctx, self.guild)
            .map(|guild| format!("#{}", guild.name))
            .unwrap_or_else(|_| format!("<unknown guild {}>", self.channel));

        match self.what {
            InteractionType::Message => format!(
                "new message from {} in {} @ \"{}\", mentions: [{}]",
                source_name, channel_name, guild_name, target_names
            ),
            InteractionType::Reaction => format!(
                "{} reacted to a message by {} in {} @ \"{}\"",
                source_name, target_names, channel_name, guild_name
            ),
        }
    }
}

pub type RelationshipStrength = f32;

#[derive(Debug, Copy, Clone)]
pub enum RelationshipChangeReason {
    Reaction,
    MessageDirectMention,
    MessageIndirectMention,
    MessageAdjacency,
    MessageBinarySequence,
}

// TODO: I think this needs to be based on the total number of nodes in the graph.
//       AlliedModders seems to be decaying a bit quick, while Together C & C++ is not quite there.
pub const RELATIONSHIP_DECAY: RelationshipStrength = -0.02;

impl RelationshipChangeReason {
    pub fn get_change_strength(&self) -> RelationshipStrength {
        match self {
            Self::Reaction => 0.1,
            Self::MessageDirectMention => 2.0,
            Self::MessageIndirectMention => 1.0,
            Self::MessageAdjacency => 0.5,
            // TODO: Increase weight back to 1.0 once implementation is fixed.
            Self::MessageBinarySequence => 0.5,
        }
    }
}

#[derive(Debug)]
pub struct RelationshipChange {
    pub source: UserId,
    pub target: UserId,
    pub reason: RelationshipChangeReason,
}

const MESSAGE_HISTORY_COUNT: usize = 5;

#[derive(Debug)]
pub struct InferenceState {
    /// Recent messages to channel, used to infer temporal proximity.
    /// Limited to `MESSAGE_HISTORY_COUNT`, latest entries at the front.
    history: VecDeque<Interaction>,
}

impl InferenceState {
    pub fn new() -> Self {
        InferenceState {
            history: VecDeque::new(),
        }
    }

    // TODO: Re-write this as a set of inference engines.
    pub fn infer(&mut self, changes: &mut Vec<RelationshipChange>, interaction: &Interaction) {
        let source = interaction.source;

        if let Some(target) = interaction.target {
            changes.push(RelationshipChange {
                source,
                target,
                reason: match interaction.what {
                    InteractionType::Reaction => RelationshipChangeReason::Reaction,
                    InteractionType::Message => RelationshipChangeReason::MessageDirectMention,
                },
            });
        }

        if interaction.what != InteractionType::Message {
            return;
        }

        for target in &interaction.other_targets {
            changes.push(RelationshipChange {
                source,
                target: *target,
                reason: RelationshipChangeReason::MessageIndirectMention,
            });
        }

        if let Some(last) = self.history.front() {
            // If the last message isn't from the same author, and was less than 2 minutes ago.
            // TODO: There might be a reasonable dynamic option here where we set the later
            //       threshold to 5x (or something) the time difference here. Discord seems
            //       to move a bit quick for these limits. Often in #sourcemod there will be
            //       a reply within 30 seconds or so to a question answered only a couple of
            //       minutes after the previous message.
            if last.source != source
                && interaction.when.duration_since(last.when).as_secs() < (60 * 2)
            {
                // Find the message before that from a different author.
                let previous = self
                    .history
                    .iter()
                    .skip(1)
                    .find(|i| i.source != last.source);

                if let Some(previous) = previous {
                    // If there was at least 10 minutes between the last message and the message
                    // before that, and we're messaging within 2 minutes, we're probably replying.
                    if last.when.duration_since(previous.when).as_secs() > (60 * 10) {
                        changes.push(RelationshipChange {
                            source,
                            target: last.source,
                            reason: RelationshipChangeReason::MessageAdjacency,
                        });
                    }
                }
            }
        }

        self.history.push_front(interaction.clone());
        self.history.truncate(MESSAGE_HISTORY_COUNT);

        // This is... gross.
        let unique_sources = self
            .history
            .iter()
            .map(|i| i.source)
            .collect::<HashSet<UserId>>();

        // TODO: This is triggering far too often, especially after the bot first starts.
        //       The original waits for the history list to be full *and* clears it each
        //       time it triggers. It'd be good to do the same here, but I'm not sure if
        //       we can without losing the single state storage. That said, if we split
        //       these up like the original, MessageAdjacency will be a lot simpler.
        if unique_sources.len() == 2 {
            let mut unique_sources = unique_sources.iter();
            let first = unique_sources.next().unwrap();
            let second = unique_sources.next().unwrap();
            let target = if source == *first { second } else { first };

            changes.push(RelationshipChange {
                source,
                target: *target,
                reason: RelationshipChangeReason::MessageBinarySequence,
            });
        }
    }
}

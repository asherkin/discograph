use anyhow::{Context as AnyhowContext, Result};
use futures::future::join_all;
use twilight_model::channel::Message;
use twilight_model::gateway::payload::incoming::ReactionAdd;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use twilight_model::id::Id;

use std::collections::{HashSet, VecDeque};
use std::fmt::{Display, Formatter};
use std::time::Instant;

use crate::cache::{Cache, CachedMessage};

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum InteractionType {
    Message,
    Reaction,
}

#[derive(Debug, Clone)]
pub struct Interaction {
    pub what: InteractionType,
    pub when: Instant,
    pub guild: Id<GuildMarker>,
    pub channel: Id<ChannelMarker>,
    pub source: Id<UserMarker>,
    pub source_is_bot: bool,
    pub target: Option<Id<UserMarker>>,
    pub other_targets: Vec<Id<UserMarker>>,
}

impl Interaction {
    pub fn new_from_message(
        message: &Message,
        referenced_message: Option<&CachedMessage>,
    ) -> Result<Self> {
        let guild_id = message
            .guild_id
            .context("tried to create an interaction from a message not sent to a guild")?;

        // We prefer a parsed mention over a reply, as a parsed mention with reply is usually
        // someone directing a message to someone else.
        let reply_to = parse_direct_mention(&message.content)
            .or_else(|| referenced_message.map(|m| m.author_id));

        let user_mentions = message
            .mentions
            .iter()
            .map(|mention| mention.id)
            .filter(|&u| Some(u) != reply_to)
            .collect::<Vec<_>>();

        Ok(Interaction {
            what: InteractionType::Message,
            when: Instant::now(),
            guild: guild_id,
            channel: message.channel_id,
            source: message.author.id,
            source_is_bot: message.author.bot,
            target: reply_to,
            other_targets: user_mentions,
        })
    }

    pub fn new_from_reaction(
        reaction: &ReactionAdd,
        target_message: &CachedMessage,
    ) -> Result<Self> {
        let guild_id = reaction
            .guild_id
            .context("tried to create an interaction from a reaction not sent to a guild")?;

        let user = &reaction
            .member
            .as_ref()
            .context("member info missing from reaction")?
            .user;

        Ok(Interaction {
            what: InteractionType::Reaction,
            when: Instant::now(),
            guild: guild_id,
            channel: reaction.channel_id,
            source: reaction.user_id,
            source_is_bot: user.bot,
            target: Some(target_message.author_id),
            other_targets: Vec::new(),
        })
    }

    async fn get_user_display_name(
        cache: &Cache,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
    ) -> String {
        // Request the member first as that'll warm the user cache.
        let member = cache.get_member(guild_id, user_id).await;

        let user = match cache.get_user(user_id).await {
            Ok(user) => user,
            Err(_) => return format!("<invalid user {}>", user_id),
        };

        let nickname = member
            .map_or(None, |member| member.nick)
            .unwrap_or_else(|| user.name.clone());

        format!("\"{}\" ({}#{:04})", nickname, user.name, user.discriminator)
    }

    pub async fn to_string(&self, cache: &Cache) -> String {
        let source_name = Self::get_user_display_name(cache, self.guild, self.source).await;

        let target_name_futures = (self.target.iter())
            .chain(self.other_targets.iter())
            .map(|&user_id| Self::get_user_display_name(cache, self.guild, user_id));

        let mut target_names = join_all(target_name_futures).await;

        let primary_target_name = if self.target.is_some() {
            Some(target_names.remove(0))
        } else {
            None
        };

        let channel_name = match cache.get_channel(self.channel).await {
            Ok(channel) => format!("#{}", channel.name),
            Err(_) => format!("<invalid channel {}>", self.channel),
        };

        let guild_name = match cache.get_guild(self.guild).await {
            Ok(guild) => guild.name,
            Err(_) => format!("<invalid guild {}>", self.guild),
        };

        match self.what {
            InteractionType::Message => format!(
                "new message from {} in {} @ \"{}\", target: {}, mentions: [{}]",
                source_name,
                channel_name,
                guild_name,
                primary_target_name.as_deref().unwrap_or("None"),
                target_names.join(", "),
            ),
            InteractionType::Reaction => format!(
                "{} reacted to a message by {} in {} @ \"{}\"",
                source_name,
                primary_target_name.unwrap(),
                channel_name,
                guild_name,
            ),
        }
    }
}

pub type RelationshipStrength = f32;

// These values are serialized and can't be modified.
#[derive(Debug, Copy, Clone)]
pub enum RelationshipChangeReason {
    Reaction = 1,
    MessageDirectMention = 2,
    MessageIndirectMention = 3,
    MessageAdjacency = 4,
    MessageBinarySequence = 5,
}

// TODO: I think this needs to be based on the total number of nodes in the graph.
//       AlliedModders seems to be decaying a bit quick, while Together C & C++ is not quite there.
pub const RELATIONSHIP_DECAY: RelationshipStrength = -0.02;
pub const RELATIONSHIP_DECAY_GLOBAL: RelationshipStrength = -0.0002;

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
    pub source: Id<UserMarker>,
    pub target: Id<UserMarker>,
    pub reason: RelationshipChangeReason,
}

impl Display for RelationshipChange {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{} --> {} [{:?}]",
            self.source, self.target, self.reason
        ))
    }
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

        if self.history.len() >= MESSAGE_HISTORY_COUNT {
            self.history.truncate(MESSAGE_HISTORY_COUNT);

            // This is... gross.
            let unique_sources = self
                .history
                .iter()
                .map(|i| i.source)
                .collect::<HashSet<Id<UserMarker>>>();

            // TODO: The original waits for the history list to be full *and* clears it each
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

                // We can't do this while MessageAdjacency is sharing the same list.
                // self.history.clear();
            }
        }
    }
}

// TODO: This isn't as good as our nom version, something to look at later.
fn parse_direct_mention(message: &str) -> Option<Id<UserMarker>> {
    let message = match message.rfind("\n>") {
        Some(v) => message.get((v + 2)..)?,
        None => message,
    };
    let message = match message.find('\n') {
        Some(v) => message.get((v + 1)..)?,
        None => message,
    };

    if !message.starts_with("<@") {
        return None;
    }

    let start = match message.get(2..3) {
        Some("!") => 3,
        _ => 2,
    };

    let end = message.find('>')?;

    let id = message.get(start..end)?;

    Id::new_checked(id.parse().ok()?)
}

#[cfg(test)]
mod parse_direct_mention_tests {
    use super::parse_direct_mention;
    use twilight_model::id::Id;

    #[test]
    fn test_simple() {
        let message = "<@!766407857851072512> hello";
        let mention = parse_direct_mention(message);
        assert_eq!(mention, Some(Id::new(766407857851072512)));
    }

    #[test]
    fn test_quoted() {
        let message = "> <@!766407857851072512> hello\n<@298220148647526402> test";
        let mention = parse_direct_mention(message);
        assert_eq!(mention, Some(Id::new(298220148647526402)));
    }

    #[test]
    fn test_multi_quote() {
        let message = "> test\n> test\n<@!766407857851072512> test";
        let mention = parse_direct_mention(message);
        assert_eq!(mention, Some(Id::new(766407857851072512)));
    }
}

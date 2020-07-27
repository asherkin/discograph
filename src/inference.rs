use petgraph::graphmap::DiGraphMap;
use petgraph::EdgeDirection;
use serenity::client::Context;
use serenity::model::prelude::*;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use crate::cache::{Cache, CachedMessage, CachedUser};
use crate::parsing::parse_direct_mention;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum InteractionType {
    Message,
    Reaction,
}

#[derive(Debug, Clone)]
pub(crate) struct Interaction {
    what: InteractionType,
    when: Instant,
    guild: GuildId,
    channel: ChannelId,
    source: UserId,
    target: Option<UserId>,
    other_targets: Vec<UserId>,
}

impl Interaction {
    pub fn new_from_message(message: &Message) -> Self {
        let direct_mention = parse_direct_mention(&message.content);

        let user_mentions = message
            .mentions
            .iter()
            .map(|u| u.id)
            .filter(|u| Some(*u) != direct_mention)
            .collect::<Vec<UserId>>();

        Interaction {
            what: InteractionType::Message,
            when: Instant::now(),
            guild: message.guild_id.unwrap(),
            channel: message.channel_id,
            source: message.author.id,
            target: direct_mention,
            other_targets: user_mentions,
        }
    }

    pub fn new_from_reaction(reaction: &Reaction, target_message: &CachedMessage) -> Self {
        Interaction {
            what: InteractionType::Reaction,
            when: Instant::now(),
            guild: reaction.guild_id.unwrap(),
            channel: reaction.channel_id,
            source: reaction.user_id,
            target: Some(target_message.author_id),
            other_targets: Vec::new(),
        }
    }

    pub fn to_string(&self, ctx: &Context, cache: &Cache) -> String {
        let user_to_display_name = |user: CachedUser, guild_id: GuildId, user_id: UserId| {
            format!(
                "\"{}\" ({}#{:04})",
                cache
                    .get_member(ctx, guild_id, user_id)
                    .unwrap()
                    .nick
                    .unwrap_or_else(|| user.name.clone()),
                user.name,
                user.discriminator
            )
        };

        let source_name = cache
            .get_user(&ctx, self.source)
            .map(|user| user_to_display_name(user, self.guild, self.source))
            .unwrap();

        let target_names = (self.target.iter())
            .chain(self.other_targets.iter())
            .map(|user_id| (*user_id, cache.get_user(&ctx, *user_id).unwrap()))
            .map(|(user_id, user)| user_to_display_name(user, self.guild, user_id))
            .collect::<Vec<String>>()
            .join(", ");

        let channel_name = format!("#{}", cache.get_channel(&ctx, self.channel).unwrap().name);

        let guild_name = cache.get_guild(&ctx, self.guild).unwrap().name;

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

const RELATIONSHIP_DECAY: RelationshipStrength = -0.0001;

impl RelationshipChangeReason {
    fn get_change_strength(&self) -> RelationshipStrength {
        match self {
            Self::Reaction => 0.1,
            Self::MessageDirectMention => 5.0,
            Self::MessageIndirectMention => 2.0,
            Self::MessageAdjacency => 0.5,
            Self::MessageBinarySequence => 1.0,
        }
    }
}

#[derive(Debug)]
pub struct RelationshipChange {
    source: UserId,
    target: UserId,
    reason: RelationshipChangeReason,
}

const MESSAGE_HISTORY_COUNT: usize = 5;

#[derive(Debug)]
struct InferenceState {
    /// Recent messages to channel, used to infer temporal proximity.
    /// Limited to `MESSAGE_HISTORY_COUNT`, latest entries at the front.
    history: VecDeque<Interaction>,
}

impl InferenceState {
    fn new() -> Self {
        InferenceState {
            history: VecDeque::new(),
        }
    }

    // TODO: Re-write this as a set of inference engines.
    fn infer(&mut self, changes: &mut Vec<RelationshipChange>, interaction: &Interaction) {
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

        // This can trigger early when the bot first starts.
        // The original waits for the history list to be full *and* clears it each time it triggers.
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

type UserRelationshipGraphMap = DiGraphMap<UserId, RelationshipStrength>;

#[derive(Debug)]
pub(crate) struct SocialGraph {
    graph: HashMap<GuildId, HashMap<ChannelId, UserRelationshipGraphMap>>,
    state: HashMap<(GuildId, ChannelId), InferenceState>,
}

impl SocialGraph {
    pub fn new() -> Self {
        SocialGraph {
            graph: HashMap::new(),
            state: HashMap::new(),
        }
    }

    /// Helper function to run inference with the right state.
    pub fn infer(&mut self, interaction: &Interaction) -> Vec<RelationshipChange> {
        let mut changes = Vec::new();

        self.state
            .entry((interaction.guild, interaction.channel))
            .or_insert_with(InferenceState::new)
            .infer(&mut changes, interaction);

        changes
    }

    /// Apply a set of relationship changes to the graph.
    pub fn apply(&mut self, interaction: &Interaction, changes: &[RelationshipChange]) {
        let graph = self.get_graph(interaction.guild, interaction.channel);

        Self::decay(graph, RELATIONSHIP_DECAY);

        for change in changes {
            match graph.edge_weight_mut(change.source, change.target) {
                Some(weight) => {
                    *weight += change.reason.get_change_strength();
                }
                None => {
                    graph.add_edge(
                        change.source,
                        change.target,
                        change.reason.get_change_strength(),
                    );
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_channel_graph(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Option<&UserRelationshipGraphMap> {
        self.graph.get(&guild_id).and_then(|g| g.get(&channel_id))
    }

    pub fn build_guild_graph(&self, guild_id: GuildId) -> Option<UserRelationshipGraphMap> {
        let guild = self.graph.get(&guild_id)?;

        let mut guild_graph = UserRelationshipGraphMap::new();
        for channel_graph in guild.values() {
            for (source, target, weight) in channel_graph.all_edges() {
                match guild_graph.edge_weight_mut(source, target) {
                    Some(new_weight) => {
                        *new_weight += weight;
                    }
                    None => {
                        guild_graph.add_edge(source, target, *weight);
                    }
                }
            }
        }

        Some(guild_graph)
    }

    fn get_graph(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> &mut UserRelationshipGraphMap {
        self.graph
            .entry(guild_id)
            .or_insert_with(HashMap::new)
            .entry(channel_id)
            .or_insert_with(DiGraphMap::new)
    }

    fn decay(graph: &mut UserRelationshipGraphMap, amount: RelationshipStrength) -> (usize, usize) {
        let mut edges_to_remove = Vec::new();
        let mut nodes_to_check = HashSet::new();

        for (source, target, relationship) in graph.all_edges_mut() {
            *relationship += amount;

            if *relationship <= 0.0 {
                edges_to_remove.push((source, target));

                nodes_to_check.insert(source);
                nodes_to_check.insert(target);
            }
        }

        let mut removed_edges = 0;
        for (source, target) in edges_to_remove {
            graph.remove_edge(source, target);
            removed_edges += 1;
        }

        let mut removed_nodes = 0;
        for node in nodes_to_check {
            // TODO: Can we do this any better?
            let has_incoming = graph
                .neighbors_directed(node, EdgeDirection::Incoming)
                .next()
                .is_none();
            let has_outgoing = graph
                .neighbors_directed(node, EdgeDirection::Outgoing)
                .next()
                .is_none();
            if !has_incoming && !has_outgoing {
                graph.remove_node(node);
                removed_nodes += 1;
            }
        }

        (removed_nodes, removed_edges)
    }
}

use serde::de::{Deserialize, Deserializer, Error as DeserializerError, MapAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serenity::client::Context;
use serenity::model::prelude::*;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::num::ParseIntError;
use std::path::{Path, PathBuf};

use crate::cache::Cache;
use crate::inference::{
    InferenceState, Interaction, RelationshipChange, RelationshipStrength, RELATIONSHIP_DECAY,
};

#[derive(Debug)]
pub struct UserRelationshipGraphMap(HashMap<(UserId, UserId), RelationshipStrength>);

impl UserRelationshipGraphMap {
    fn new() -> Self {
        UserRelationshipGraphMap(HashMap::new())
    }

    fn new_from_path(path: &Path) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;

        Ok(serde_json::from_str(&contents)?)
    }

    fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        if self.0.is_empty() {
            return Ok(());
        }

        let contents = serde_json::to_string(self)?;

        let mut file = File::create(path)?;
        file.write_all(contents.as_bytes())
    }

    fn decay(&mut self, amount: RelationshipStrength) {
        let mut edges_to_remove = Vec::new();

        for (&source_target, relationship) in self.iter_mut() {
            *relationship += amount;

            if *relationship <= 0.0 {
                edges_to_remove.push(source_target);
            }
        }

        for source_target in edges_to_remove {
            self.remove(&source_target);
        }
    }

    pub fn to_dot(&self, ctx: &Context, cache: &Cache) -> String {
        // Gather all undirected edges.
        let mut undirected_edges = HashMap::new();
        for (&(source, target), new_weight) in &self.0 {
            // Ignore self-connected edges.
            if source == target {
                continue;
            }

            // Sort the key to make it direction-independent.
            let mut key = [source, target];
            key.sort();

            // As we're collapsing directed edges, we need to sum the weights.
            let weight: &mut RelationshipStrength = undirected_edges.entry(key).or_default();
            *weight += new_weight;
        }

        // Remove any edges that have a weight under the threshold and build a list of unique user IDs.
        let mut user_ids = HashSet::new();
        undirected_edges.retain(|&[source, target], weight| {
            if *weight > 1.0 {
                user_ids.insert(source);
                user_ids.insert(target);

                true
            } else {
                false
            }
        });

        // Get the username for each user ID, ignoring failed lookups or bots.
        // TODO: This can be *very* slow if the user isn't in the cache.
        let usernames: HashMap<UserId, String> = user_ids
            .iter()
            .filter_map(|&user_id| {
                let user = cache.get_user(&ctx, user_id).ok()?;
                if user.bot {
                    return None;
                }

                Some((user_id, user.name))
            })
            .collect();

        // Filter any edges that were to bots or we couldn't lookup.
        undirected_edges.retain(|[source, target], _| {
            usernames.contains_key(source) && usernames.contains_key(target)
        });

        let mut lines = Vec::with_capacity(6 + usernames.len() + undirected_edges.len() + 1);

        lines.push(String::from("graph {"));
        lines.push(String::from("    layout = \"fdp\""));
        lines.push(String::from("    K = \"0.05\""));
        lines.push(String::from("    splines = \"true\""));
        lines.push(String::from("    overlap = \"30:true\""));
        lines.push(String::from("    edge [ fontsize = \"0\" ]"));

        for (user_id, name) in usernames {
            lines.push(format!(
                "    {} [ label = \"{}\" ]",
                user_id,
                name.replace("\"", "\\\""),
            ));
        }

        for (key, weight) in undirected_edges {
            lines.push(format!(
                "    {} -- {} [ label = \"{:.2}\" ]",
                key[0], key[1], weight,
            ));
        }

        lines.push(String::from("}"));

        lines.join("\n")
    }
}

impl std::ops::Deref for UserRelationshipGraphMap {
    type Target = HashMap<(UserId, UserId), RelationshipStrength>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for UserRelationshipGraphMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Serialize for UserRelationshipGraphMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            let k = format!("{}:{}", k.0, k.1);
            map.serialize_entry(&k, v)?;
        }
        map.end()
    }
}

struct UserRelationshipGraphMapVisitor();

impl UserRelationshipGraphMapVisitor {
    fn new() -> Self {
        UserRelationshipGraphMapVisitor()
    }
}

impl<'de> Visitor<'de> for UserRelationshipGraphMapVisitor {
    // The type that our Visitor is going to produce.
    type Value = UserRelationshipGraphMap;

    // Format a message stating what data this Visitor expects to receive.
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a very special map")
    }

    // Deserialize UserRelationshipGraphMapVisitor from an abstract "map" provided by the
    // Deserializer. The MapAccess input is a callback provided by
    // the Deserializer to let us see each entry in the map.
    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut map =
            UserRelationshipGraphMap(HashMap::with_capacity(access.size_hint().unwrap_or(0)));

        // While there are entries remaining in the input, add them into our map.
        while let Some((key, value)) = access.next_entry::<&str, RelationshipStrength>()? {
            let err = "expected exactly 2 numbers separated by :";

            let mut iter = key.split(':');
            let mut get_int = || {
                iter.next()
                    .ok_or_else(|| M::Error::custom(err))?
                    .parse()
                    .map_err(|e: ParseIntError| M::Error::custom(e.to_string()))
            };
            let k1 = get_int()?;
            let k2 = get_int()?;
            if iter.next().is_some() {
                return Err(M::Error::custom(err));
            }

            map.insert((UserId(k1), UserId(k2)), value);
        }

        Ok(map)
    }
}

// This is the trait that informs Serde how to deserialize MyMap.
impl<'de> Deserialize<'de> for UserRelationshipGraphMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Instantiate our Visitor and ask the Deserializer to drive
        // it over the input data, resulting in an instance of MyMap.
        deserializer.deserialize_map(UserRelationshipGraphMapVisitor::new())
    }
}

// TODO: Just keeping this note here, but it is a rather general thing - we've got a lot of HashMap
//       objects around using Discord snowflakes as keys, which are out of user control and thus do
//       not need secure, anti-DoS hashing. We could probably increase HashMap performance a tonne
//       by avoiding the default secure hashing and just truncating them to fit / XOR for tuples.
#[derive(Debug)]
pub struct SocialGraph {
    data_dir: Option<PathBuf>,
    graph: HashMap<GuildId, HashMap<ChannelId, UserRelationshipGraphMap>>,
    state: HashMap<(GuildId, ChannelId), InferenceState>,
}

impl SocialGraph {
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        SocialGraph {
            data_dir,
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
        let data_dir = self.data_dir.clone();
        let guild_id = interaction.guild;
        let channel_id = interaction.channel;

        let graph = self.get_graph(guild_id, channel_id);

        graph.decay(RELATIONSHIP_DECAY);

        for change in changes {
            let weight = graph.entry((change.source, change.target)).or_default();

            *weight += change.reason.get_change_strength();
        }

        if let Some(data_dir) = data_dir {
            // TODO: Do we want to be writing this every update?
            // TODO: Maybe we should use a proper database for the backing store? for all of this?
            let data_path = Self::graph_data_file_name(data_dir, guild_id, channel_id);
            if let Err(err) = graph.save_to_path(&data_path) {
                eprintln!(
                    "failed to store on-disk data for ({}, {}): {}",
                    interaction.guild, interaction.channel, err,
                );
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

    // TODO: Do we want to do this on the client-side instead? Probably.
    pub fn build_guild_graph(&self, guild_id: GuildId) -> Option<UserRelationshipGraphMap> {
        let guild = self.graph.get(&guild_id)?;

        let mut guild_graph = UserRelationshipGraphMap::new();
        for channel_graph in guild.values() {
            for (&source_target, weight) in channel_graph.iter() {
                let guild_weight = guild_graph.entry(source_target).or_default();

                *guild_weight += weight;
            }
        }

        Some(guild_graph)
    }

    // TODO: Temporary hack for debug command.
    pub fn get_all_guild_ids(&self) -> Vec<&GuildId> {
        self.graph.keys().collect()
    }

    pub(crate) fn get_graph(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> &mut UserRelationshipGraphMap {
        let data_dir = self.data_dir.clone();

        self.graph
            .entry(guild_id)
            .or_insert_with(HashMap::new)
            .entry(channel_id)
            .or_insert_with(|| {
                let existing_graph = data_dir.and_then(|data_dir| {
                    let data_path = Self::graph_data_file_name(data_dir, guild_id, channel_id);
                    match UserRelationshipGraphMap::new_from_path(&data_path) {
                        Ok(graph) => Some(graph),
                        Err(err) if err.kind() == IoErrorKind::NotFound => None,
                        Err(err) => {
                            eprintln!(
                                "failed to load on-disk data for ({}, {}): {}",
                                guild_id, channel_id, err,
                            );

                            None
                        }
                    }
                });

                existing_graph.unwrap_or_else(UserRelationshipGraphMap::new)
            })
    }

    fn graph_data_file_name(
        data_dir: PathBuf,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> PathBuf {
        let mut file_name = data_dir;
        file_name.push(format!("{}_{}.json", guild_id, channel_id));
        file_name
    }
}

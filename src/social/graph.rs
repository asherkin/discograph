use anyhow::Result as AnyhowResult;
use futures::future::join_all;
use serde::de::{Deserialize, Deserializer, Error as DeserializerError, MapAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use tracing::{error, info, warn};
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use twilight_model::id::Id;
use twilight_model::user::User;
use unicode_segmentation::UnicodeSegmentation;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::num::ParseIntError;
use std::path::{Path, PathBuf};

use super::inference::{
    InferenceState, Interaction, RelationshipChange, RelationshipStrength, RELATIONSHIP_DECAY,
};
use crate::cache::CachedMember;
use crate::context::Context;
use crate::social::inference::{InteractionType, RELATIONSHIP_DECAY_GLOBAL};

// TODO: This doesn't handle counting wide characters very well,
//       Probably want to pull in the unicode-width crate for that.
fn get_label(mut name: String) -> String {
    let label_length = 9;
    let ellipsis = "...";
    let ellipsis_length = ellipsis.len();

    let mut name_iter = name.grapheme_indices(true);
    let key_graphemes = (
        name_iter.nth(label_length - ellipsis_length),
        name_iter.nth(ellipsis_length - 1),
    );

    match key_graphemes {
        (_, None) => name,
        (Some((pos, _)), Some(_)) => {
            name.truncate(pos);
            name.push_str(ellipsis);
            name
        }
        (None, Some(_)) => unreachable!(),
    }
}

fn calculate_luma(color: u32) -> f32 {
    // TODO: Consider alpha blending with a background color.
    let r = ((color >> 24) & 0xFF) as f32;
    let g = ((color >> 16) & 0xFF) as f32;
    let b = ((color >> 8) & 0xFF) as f32;

    (r * 0.299) + (g * 0.587) + (b * 0.114)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ColorScheme {
    Light,
    Dark,
}

#[derive(Clone, Debug)]
pub struct UserRelationshipGraphMap(
    HashMap<(Id<UserMarker>, Id<UserMarker>), RelationshipStrength>,
);

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

    pub async fn to_dot(
        &self,
        context: &Context,
        guild_id: Id<GuildMarker>,
        requesting_user: Option<&User>,
        color_scheme: ColorScheme,
        transparent: bool,
        font_name: &str,
    ) -> AnyhowResult<String> {
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
            if *weight >= 1.0 {
                user_ids.insert(source);
                user_ids.insert(target);

                true
            } else {
                false
            }
        });

        // Load all color-affecting roles for the guild.
        let roles = {
            let role_futures = context
                .cache
                .get_guild(guild_id)
                .await?
                .roles
                .into_iter()
                .map(|role_id| context.cache.get_role(guild_id, role_id));

            let mut roles: Vec<_> = join_all(role_futures)
                .await
                .into_iter()
                .filter_map(|role| match role {
                    Ok(role) if role.color != 0 => Some(role),
                    _ => None,
                })
                .collect();

            roles.sort_unstable_by_key(|role| std::cmp::Reverse(role.position));

            roles
        };

        // Get the display name for each user ID, ignoring failed lookups or bots.
        let names_and_colors: HashMap<_, _> = {
            let (_, not_found) = context
                .cache
                .bulk_preload_members(&context.shard, guild_id, user_ids.iter().cloned())
                .await?;

            let not_found: HashSet<_> = not_found.into_iter().collect();

            let roles = &roles;
            let not_found = &not_found;
            let futures = user_ids.iter().map(|&user_id| async move {
                // Query the member first as it might populate the cache for the user.
                // Avoid querying for users that our bulk fetch explicitly detected as missing.

                let member = if !not_found.contains(&user_id) {
                    match context.cache.get_member(guild_id, user_id).await {
                        Ok(user) => Some(user),
                        Err(error) => {
                            info!(
                                ?guild_id,
                                ?user_id,
                                ?error,
                                "failed to fetch member from cache"
                            );

                            None
                        }
                    }
                } else {
                    None
                };

                // TODO: Decide if we should just remove users without member info from the graph.
                //       It generally means they've left the guild, and if they've deleted their
                //       account Discord doesn't like us repeatedly querying them either.
                let user = match context.cache.get_user(user_id).await {
                    Ok(user) => user,
                    Err(error) => {
                        warn!(
                            ?guild_id,
                            ?user_id,
                            ?error,
                            "failed to fetch user from cache"
                        );

                        return None;
                    }
                };

                if user.bot {
                    return None;
                }

                let is_member = member.is_some();

                let color = member.as_ref().and_then(|member| {
                    let member_roles: HashSet<_> = member.roles.iter().cloned().collect();

                    roles.iter().find_map(|role| {
                        if member_roles.contains(&role.id) {
                            Some(role.color)
                        } else {
                            None
                        }
                    })
                });

                let name = if let Some(CachedMember {
                    nick: Some(nick), ..
                }) = member
                {
                    nick
                } else {
                    user.name
                };

                Some((user_id, (name, color, is_member)))
            });

            join_all(futures).await.into_iter().flatten().collect()
        };

        // Filter any edges that were to bots or we couldn't lookup and sum per-user weights.
        let mut user_weights: HashMap<Id<UserMarker>, RelationshipStrength> = HashMap::new();
        undirected_edges.retain(|[source, target], weight| {
            let retain =
                names_and_colors.contains_key(source) && names_and_colors.contains_key(target);

            if retain {
                let source_weight = user_weights.entry(*source).or_default();
                *source_weight += *weight;

                let target_weight = user_weights.entry(*target).or_default();
                *target_weight += *weight;
            }

            retain
        });

        if user_weights.is_empty() {
            anyhow::bail!("Not enough users to create a graph");
        }

        const BG_LIGHT: u32 = 0xFFFFFFFF;
        const FG_LIGHT: u32 = 0x060607FF;
        const BG_DARK: u32 = 0x313338FF;
        const FG_DARK: u32 = 0xF2F3F5FF;

        let (bg_color, fg_color) = match color_scheme {
            ColorScheme::Light => (BG_LIGHT, FG_LIGHT),
            ColorScheme::Dark => (BG_DARK, FG_DARK),
        };

        let mut lines = Vec::with_capacity(17 + user_weights.len() + undirected_edges.len() + 1);

        lines.push(String::from("graph {"));
        lines.push(String::from("    dpi = \"144\""));
        lines.push(String::from("    pad = \"0.3\""));
        lines.push(String::from("    layout = \"fdp\""));
        lines.push(String::from("    K = \"0.1\""));
        lines.push(String::from("    splines = \"true\""));
        lines.push(String::from("    overlap = \"30:true\""));
        lines.push(String::from("    outputorder = \"edgesfirst\""));
        lines.push(String::from("    truecolor = \"true\""));
        lines.push(format!("    color = \"#{:08X}\"", fg_color));
        lines.push(format!("    fontcolor = \"#{:08X}\"", fg_color));

        if transparent {
            lines.push(String::from("    bgcolor = \"transparent\""));
        } else {
            lines.push(format!("    bgcolor = \"#{:08X}\"", bg_color));
        }

        if let Some(user) = requesting_user {
            let guild = context.cache.get_guild(guild_id).await?;

            let member = context
                .cache
                .get_member(guild_id, context.user.id)
                .await
                .ok();

            let nickname = match &member {
                Some(CachedMember {
                    nick: Some(nick), ..
                }) => nick,
                _ => &context.user.name,
            };

            let safe_name = user.name.replace('\\', "\\\\").replace('"', "\\\"");
            let safe_nickname = nickname.replace('\\', "\\\\").replace('"', "\\\"");
            let safe_guild_name = guild.name.replace('\\', "\\\\").replace('"', "\\\"");

            // TODO: Add a timestamp.
            let label = format!(
                "Generated for {}#{:04} by {} in {}",
                safe_name, user.discriminator, safe_nickname, safe_guild_name,
            );

            lines.push(format!("    label = \"{}\"", label));
            lines.push(String::from("    labelloc = \"bottom\""));
            lines.push(String::from("    labeljust = \"left\""));
            lines.push(format!("    fontname = \"{}\"", font_name));
        }

        lines.push(format!("    node [ fontname = \"{}\" ]", font_name));

        for (user_id, weight) in &user_weights {
            let (name, role_color, is_member) = names_and_colors.get(user_id).unwrap().clone();
            let width = 1.0 + weight.log10();

            // TODO: This could be a lot more efficient.
            let mut label = get_label(name.to_owned())
                .replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('\'', "&#x27;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('\\', "\\\\");

            let mut peripheries = 1;
            let mut color = fg_color;
            let mut fillcolor = bg_color;
            let mut fontcolor = fg_color;

            if let Some(role_color) = role_color {
                color = (role_color << 8) | 0xFF;
            }

            if !is_member {
                color -= 200;
                fontcolor -= 200;
            }

            if let Some(user) = requesting_user {
                // Invert the colors if it is the requesting user.
                if *user_id == user.id {
                    // Make the text bold.
                    label = format!("<B>{}</B>", label);

                    peripheries = 2;

                    fillcolor = color;

                    // Select text color based on fill contrast.
                    fontcolor = if calculate_luma(fillcolor) > 186.0 {
                        FG_LIGHT
                    } else {
                        FG_DARK
                    };
                }
            }

            lines.push(format!(
                "    {} [ label = <{}>, penwidth = \"{}\", style = \"filled\", peripheries = \"{}\", color = \"#{:08X}\", fillcolor = \"#{:08X}\", fontcolor = \"#{:08X}\" ]",
                user_id,
                label,
                width,
                peripheries,
                color,
                fillcolor,
                fontcolor,
            ));
        }

        for ([source, target], weight) in undirected_edges {
            let width = 1.0 + weight.log10();
            let mut color = fg_color;

            // TODO: We could store this in undirected_edges.
            let (_, _, source_is_member) = names_and_colors.get(&source).unwrap();
            let (_, _, target_is_member) = names_and_colors.get(&target).unwrap();
            if !source_is_member || !target_is_member {
                color -= 200;
            }

            lines.push(format!(
                "    {} -- {} [ weight = \"{}\", penwidth = \"{}\", color = \"#{:08X}\" ]",
                source, target, weight, width, color,
            ));
        }

        lines.push(String::from("}"));

        Ok(lines.join("\n"))
    }
}

impl std::ops::Deref for UserRelationshipGraphMap {
    type Target = HashMap<(Id<UserMarker>, Id<UserMarker>), RelationshipStrength>;

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

            map.insert((Id::new(k1), Id::new(k2)), value);
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
    graph: HashMap<Id<GuildMarker>, HashMap<Id<ChannelMarker>, UserRelationshipGraphMap>>,
    state: HashMap<(Id<GuildMarker>, Id<ChannelMarker>), InferenceState>,
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

        // Decay all of the guild channel's graphs a tiny bit.
        if interaction.what == InteractionType::Message && !interaction.source_is_bot {
            if let Some(guild_graphs) = self.graph.get_mut(&guild_id) {
                for graph in guild_graphs.values_mut() {
                    graph.decay(RELATIONSHIP_DECAY_GLOBAL);
                }
            }
        }

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
                error!(
                    "failed to store on-disk data for ({}, {}): {}",
                    interaction.guild, interaction.channel, err,
                );
            }
        }
    }

    // TODO: Do we want to do this on the client-side instead? Probably.
    pub fn build_guild_graph(&self, guild_id: Id<GuildMarker>) -> Option<UserRelationshipGraphMap> {
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
    pub fn get_all_guild_ids(&self) -> Vec<(Id<GuildMarker>, usize)> {
        let mut guilds: Vec<_> = self
            .graph
            .iter()
            .map(|(guild_id, channels)| {
                (*guild_id, channels.values().map(|graph| graph.len()).sum())
            })
            .collect();

        guilds.sort_by_key(|(_, len)| *len);
        guilds.reverse();

        guilds
    }

    pub(crate) fn get_graph(
        &mut self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
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
                            error!(
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

    pub fn remove_guild(&mut self, guild_id: Id<GuildMarker>) {
        let channels = self.graph.remove(&guild_id);

        if let Some(channels) = channels {
            for &channel_id in channels.keys() {
                self.state.remove(&(guild_id, channel_id));
            }
        }
    }

    pub fn remove_channel(&mut self, guild_id: Id<GuildMarker>, channel_id: Id<ChannelMarker>) {
        self.state.remove(&(guild_id, channel_id));

        if let Some(channels) = self.graph.get_mut(&guild_id) {
            channels.remove(&channel_id);
        }
    }

    fn graph_data_file_name(
        data_dir: PathBuf,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> PathBuf {
        let mut file_name = data_dir;
        file_name.push(format!("{}_{}.json", guild_id, channel_id));
        file_name
    }
}

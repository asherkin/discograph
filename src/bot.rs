use log::info;
use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::{Context, EventHandler, Mutex, RwLock};
use serenity::Result as SerenityResult;

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex as StdMutex};

use crate::cache::Cache;
use crate::inference::Interaction;
use crate::parsing::Command;
use crate::social::SocialGraph;

struct DropNotifier(Arc<(StdMutex<bool>, Condvar)>);

impl Drop for DropNotifier {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        let mut finished = lock.lock().unwrap();
        *finished = true;
        cvar.notify_one();
    }
}

pub struct Handler {
    owners: RwLock<HashSet<UserId>>,
    user: RwLock<Option<CurrentUser>>,
    cache: Cache,
    social: Mutex<SocialGraph>,
    _notification: DropNotifier,
}

impl Handler {
    pub fn new(data_dir: Option<PathBuf>, notification: Arc<(StdMutex<bool>, Condvar)>) -> Self {
        Handler {
            owners: RwLock::new(HashSet::new()),
            user: RwLock::new(None),
            cache: Cache::new(),
            social: Mutex::new(SocialGraph::new(data_dir)),
            _notification: DropNotifier(notification),
        }
    }

    fn process_interaction(&self, ctx: &Context, interaction: Interaction) {
        info!("{}", interaction.to_string(&ctx, &self.cache));

        let mut social = self.social.lock();

        let changes = social.infer(&interaction);
        for change in &changes {
            info!("-> {:?}", change);
        }

        social.apply(&interaction, &changes);
    }

    fn add_help_embed(&self, reply_to: &Message, message: &mut CreateMessage) {
        let (our_id, our_name) = {
            let info = self.user.read();
            let info = info.as_ref().unwrap();
            (info.id, info.name.clone())
        };

        // let graph_link_text = "online interactive graph";
        // let graph_link = reply_to
        //     .guild_id
        //     .map_or(graph_link_text.to_string(), |guild_id| {
        //         // TODO
        //         format!(
        //             "[{}](https://google.com/search?q={})",
        //             graph_link_text, guild_id,
        //         )
        //     });
        // let link_help_line = format!("` link  `\u{2000}Get a link to the {}.", graph_link,);

        let invite_url = format!(
            "https://discord.com/api/oauth2/authorize?client_id={}&permissions=117824&scope=bot",
            our_id,
        );

        message.embed(|embed| {
            embed.description(format!("I'm a Discord Bot that infers relationships between users and draws pretty graphs.\nI'll only respond to messages that directly mention me, like `@{} help`.", our_name))
                .field("Commands", vec![
                    "` help  `\u{2000}This message.",
                    // &link_help_line,
                    "` graph `\u{2000}Get a preview-quality graph image.",
                ].join("\n"), false)
                .field("Want graphs for your guild?", format!("[Click here]({}) to invite the bot to join your server.", invite_url), false)
                .footer(|footer| {
                    footer.text(format!("Sent in response to a command from {}#{:04}", reply_to.author.name, reply_to.author.discriminator))
                })
        });
    }

    // Based on the logic in Guild._user_permissions_in
    // TODO: This should probably be in Cache or somewhere new, not here?
    fn calculate_permissions_for_user(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        user_id: UserId,
        channel_id: ChannelId,
    ) -> SerenityResult<Permissions> {
        let guild = self.cache.get_guild(&ctx, guild_id)?;

        // The owner has all permissions in all cases.
        if user_id == guild.owner_id {
            return Ok(Permissions::all());
        }

        // Start by retrieving the @everyone role's permissions.
        let everyone = self.cache.get_role(&ctx, guild_id, RoleId(guild_id.0))?;

        // Create a base set of permissions, starting with `@everyone`s.
        let mut permissions = everyone.permissions;

        let member = match self.cache.get_member(&ctx, guild_id, user_id) {
            Ok(member) => member,
            // TODO: This is from Serenity, but doesn't seem right - surely the channel overrides need accounting for?
            Err(_) => return Ok(everyone.permissions),
        };

        for &role_id in &member.roles {
            // TODO: Should this be less fatal?
            let role = self.cache.get_role(&ctx, guild_id, role_id)?;

            permissions |= role.permissions;
        }

        // Administrators have all permissions in any channel.
        if permissions.contains(Permissions::ADMINISTRATOR) {
            return Ok(Permissions::all());
        }

        let channel = self.cache.get_channel(&ctx, channel_id)?;

        // If this is a text channel, then throw out voice permissions.
        if channel.kind == ChannelType::Text {
            permissions &= !(Permissions::CONNECT
                | Permissions::SPEAK
                | Permissions::MUTE_MEMBERS
                | Permissions::DEAFEN_MEMBERS
                | Permissions::MOVE_MEMBERS
                | Permissions::USE_VAD);
        }

        // Apply the permission overwrites for the channel for each of the
        // overwrites that - first - applies to the member's roles, and then
        // the member itself.
        //
        // First apply the denied permission overwrites for each, then apply
        // the allowed.

        let mut data = Vec::with_capacity(member.roles.len());

        // Roles
        for overwrite in &channel.permission_overwrites {
            if let PermissionOverwriteType::Role(role_id) = overwrite.kind {
                if role_id.0 != guild_id.0 && !member.roles.contains(&role_id) {
                    continue;
                }

                // TODO: Should this be less fatal?
                let role = self.cache.get_role(&ctx, guild_id, role_id)?;

                data.push((role.position, overwrite.deny, overwrite.allow));
            }
        }

        data.sort_by(|a, b| a.0.cmp(&b.0));

        for overwrite in data {
            permissions = (permissions & !overwrite.1) | overwrite.2;
        }

        // Member
        for overwrite in &channel.permission_overwrites {
            if PermissionOverwriteType::Member(user_id) != overwrite.kind {
                continue;
            }

            permissions = (permissions & !overwrite.deny) | overwrite.allow;
        }

        // The default channel is always readable.
        if channel_id.0 == guild_id.0 {
            permissions |= Permissions::READ_MESSAGES;
        }

        // No SEND_MESSAGES => no message-sending-related actions
        // If the member does not have the `SEND_MESSAGES` permission, then
        // throw out message-able permissions.
        if !permissions.contains(Permissions::SEND_MESSAGES) {
            permissions &= !(Permissions::SEND_TTS_MESSAGES
                | Permissions::MENTION_EVERYONE
                | Permissions::EMBED_LINKS
                | Permissions::ATTACH_FILES);
        }

        // If the permission does not have the `READ_MESSAGES` permission, then
        // throw out actionable permissions.
        if !permissions.contains(Permissions::READ_MESSAGES) {
            permissions &= Permissions::KICK_MEMBERS
                | Permissions::BAN_MEMBERS
                | Permissions::ADMINISTRATOR
                | Permissions::MANAGE_GUILD
                | Permissions::CHANGE_NICKNAME
                | Permissions::MANAGE_NICKNAMES;
        }

        Ok(permissions)
    }
}

// noinspection RsSortImplTraitMembers
impl EventHandler for Handler {
    fn ready(&self, ctx: Context, data: Ready) {
        ctx.set_activity(Activity::watching(&format!("| @{} help", data.user.name)));
        self.user.write().replace(data.user);

        if let Ok(info) = ctx.http.get_current_application_info() {
            self.owners.write().insert(info.owner.id);
        }
    }

    fn guild_create(&self, _ctx: Context, guild: Guild) {
        self.cache.put_full_guild(&guild);

        // Load any existing graphs into memory for the guild's channels.
        let mut social = self.social.lock();
        for &channel_id in guild.channels.keys() {
            social.get_graph(guild.id, channel_id);
        }
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

            // Load any existing graph into memory for the channel.
            let mut social = self.social.lock();
            social.get_graph(channel.guild_id, channel.id);
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

        let our_id = self.user.read().as_ref().unwrap().id;
        if new_message.author.id == our_id {
            return;
        }

        // TODO: It would be good to handle commands from DMs as well.
        if new_message.mentions_user_id(our_id) {
            if let Some(command) = Command::new_from_message(our_id, &new_message.content) {
                match command {
                    Command::Link | Command::Help => {
                        new_message
                            .channel_id
                            .send_message(&ctx, |message| {
                                self.add_help_embed(&new_message, message);
                                message
                            })
                            .unwrap();
                    }
                    Command::Graph(request_channel) => {
                        let guild_id = new_message.guild_id.unwrap();
                        let guild_name = self.cache.get_guild(&ctx, guild_id).unwrap().name;

                        info!(
                            "\"{}#{:04}\" requested a graph for \"{}\"",
                            new_message.author.name, new_message.author.discriminator, guild_name,
                        );

                        new_message.channel_id.broadcast_typing(&ctx).unwrap();

                        let graph = {
                            let social = self.social.lock();

                            match request_channel {
                                Some(channel_id) => {
                                    // Do not allow requesting a graph for a channel the user can't see.
                                    let permissions = self
                                        .calculate_permissions_for_user(&ctx, guild_id, new_message.author.id, channel_id)
                                        .unwrap_or_else(|err| {
                                            info!(
                                                "Failed to calculate permissions for user {} and channel {} in {}: {:?}",
                                                new_message.author.id, channel_id, guild_id, err,
                                            );

                                            Permissions::empty()
                                        });

                                    if !permissions.read_messages() {
                                        new_message.reply(&ctx, "You can't request a graph of a channel you can't read").unwrap();

                                        return;
                                    }

                                    social.get_channel_graph(guild_id, channel_id).cloned()
                                }
                                None => social.build_guild_graph(guild_id),
                            }
                            .unwrap()
                        };

                        let dot = match graph.to_dot(
                            &ctx,
                            &self.cache,
                            guild_id,
                            Some(&new_message.author),
                        ) {
                            Ok(dot) => dot,
                            Err(err) => {
                                new_message.reply(&ctx, err).unwrap();

                                return;
                            }
                        };

                        let mut graphviz = std::process::Command::new("dot")
                            .arg("-v")
                            .arg("-Tpng")
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn()
                            .unwrap();

                        {
                            let stdin = graphviz.stdin.as_mut().unwrap();
                            stdin.write_all(dot.as_bytes()).unwrap();
                        }

                        let output = graphviz.wait_with_output().unwrap();

                        if !output.status.success() {
                            new_message.react(&ctx, '\u{274C}').unwrap();
                            return;
                        }

                        let output_name = format!("{}.png", guild_name);
                        let png: &[u8] = output.stdout.as_ref();

                        new_message
                            .channel_id
                            .send_files(&ctx, vec![(png, output_name.as_ref())], |m| {
                                m.content(new_message.author.mention())
                            })
                            .unwrap();
                    }
                    Command::Stats => {
                        new_message
                            .reply(&ctx, format!("{:?}", self.cache.get_stats()))
                            .unwrap();
                    }
                    Command::Dump => {
                        let is_owner = self.owners.read().contains(&new_message.author.id);
                        if !is_owner {
                            new_message
                                .reply(&ctx, "You don't have permission to do that")
                                .unwrap();
                            return;
                        }

                        let all_guild_ids = {
                            let social = self.social.lock();
                            social.get_all_guild_ids()
                        };

                        let mut files = Vec::new();
                        for guild_id in all_guild_ids {
                            new_message.channel_id.broadcast_typing(&ctx).unwrap();
                            info!("building guild graph for {}", guild_id);

                            let graph = {
                                let social = self.social.lock();
                                social.build_guild_graph(guild_id).unwrap()
                            };

                            match graph.to_dot(&ctx, &self.cache, guild_id, None) {
                                Ok(dot) => {
                                    let guild_name =
                                        self.cache.get_guild(&ctx, guild_id).unwrap().name;
                                    files.push((dot, format!("{}.dot", guild_name)));
                                }
                                Err(err) => {
                                    info!("Failed to create graph: {}", err);
                                }
                            }
                        }

                        let files: Vec<(&[u8], &str)> = files
                            .iter()
                            .map(|(contents, name)| (contents.as_bytes(), name.as_ref()))
                            .collect();

                        for to_send in files.chunks(10) {
                            new_message
                                .channel_id
                                .send_files(&ctx, to_send.to_vec(), |m| m)
                                .unwrap();
                        }
                    }
                    Command::Unknown(command) => {
                        new_message
                            .channel_id
                            .send_message(&ctx, |message| {
                                message.content(format!("Unknown command: {}", command));
                                self.add_help_embed(&new_message, message);
                                message
                            })
                            .unwrap();
                    }
                };

                return;
            }
        }

        self.cache.put_message(&new_message);

        // This needs to be done after putting the message in the cache
        // as we need to know it when handling reactions.
        if new_message.kind != MessageType::Regular {
            return;
        }

        let interaction = Interaction::new_from_message(&new_message).unwrap();
        self.process_interaction(&ctx, interaction);
    }

    fn reaction_add(&self, ctx: Context, add_reaction: Reaction) {
        if add_reaction.guild_id.is_none() || add_reaction.member.is_none() {
            return;
        }

        let our_id = self.user.read().as_ref().unwrap().id;
        if add_reaction.user_id == our_id {
            return;
        }

        self.cache.put_member(
            add_reaction.guild_id.unwrap(),
            add_reaction.user_id,
            add_reaction.member.as_ref().unwrap(),
        );

        match self
            .cache
            .get_message(&ctx, add_reaction.channel_id, add_reaction.message_id)
        {
            Ok(message_info) => {
                if message_info.kind != MessageType::Regular {
                    return;
                }

                let interaction =
                    Interaction::new_from_reaction(&add_reaction, &message_info).unwrap();
                self.process_interaction(&ctx, interaction);
            }
            Err(err) => {
                info!("Failed to load message for reaction: {:?}", err);
            }
        }
    }
}

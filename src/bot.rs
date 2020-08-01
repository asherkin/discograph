use serenity::builder::CreateMessage;
use serenity::model::prelude::*;
use serenity::prelude::{Context, EventHandler, Mutex, RwLock};

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use crate::cache::Cache;
use crate::inference::Interaction;
use crate::parsing::Command;
use crate::social::SocialGraph;

#[derive(Debug, Eq, PartialEq)]
pub enum BotEnvironment {
    Development,
    Production,
}

pub struct Handler {
    environment: BotEnvironment,
    user: RwLock<Option<CurrentUser>>,
    cache: Cache,
    social: Mutex<SocialGraph>,
}

impl Handler {
    pub fn new(data_dir: Option<PathBuf>, environment: BotEnvironment) -> Self {
        Handler {
            environment,
            user: RwLock::new(None),
            cache: Cache::new(),
            social: Mutex::new(SocialGraph::new(data_dir)),
        }
    }

    fn process_interaction(&self, ctx: &Context, interaction: Interaction) {
        println!("{}", interaction.to_string(&ctx, &self.cache));

        let mut social = self.social.lock();

        let changes = social.infer(&interaction);
        for change in &changes {
            println!("-> {:?}", change);
        }

        social.apply(&interaction, &changes);
    }

    fn add_help_embed(&self, reply_to: &Message, message: &mut CreateMessage) {
        let (our_id, our_name) = {
            let info = self.user.read();
            let info = info.as_ref().unwrap();
            (info.id, info.name.clone())
        };

        let graph_link_text = "online interactive graph";
        let graph_link = reply_to
            .guild_id
            .map_or(graph_link_text.to_string(), |guild_id| {
                // TODO
                format!(
                    "[{}](https://google.com/search?q={})",
                    graph_link_text, guild_id,
                )
            });
        let link_help_line = format!("` link  `\u{2000}Get a link to the {}.", graph_link,);

        let invite_url = format!(
            "https://discord.com/api/oauth2/authorize?client_id={}&permissions=117824&scope=bot",
            our_id,
        );

        message.embed(|embed| {
            embed.description(format!("I'm a Discord Bot that infers relationships between users and draws pretty graphs.\nI'll only respond to messages that directly mention me, like `@{} help`.", our_name))
                .field("Commands", vec![
                    "` help  `\u{2000}This message.",
                    &link_help_line,
                    "` graph `\u{2000}Get a preview-quality graph image.",
                ].join("\n"), false)
                .field("Want graphs for your guild?", format!("[Click here]({}) to invite the bot to join your server.", invite_url), false)
                .footer(|footer| {
                    footer.text(format!("Sent in response to a command from {}#{:04}", reply_to.author.name, reply_to.author.discriminator))
                })
        });
    }
}

// noinspection RsSortImplTraitMembers
impl EventHandler for Handler {
    fn ready(&self, ctx: Context, data: Ready) {
        ctx.set_activity(Activity::watching(&format!("| @{} help", data.user.name)));
        self.user.write().replace(data.user);

        // TODO: Send a message to all instances of ourself for coordination.
        //       This needs to come from configuration - and needs a lot of work.
        //       It is probably worthwhile to do though as we'll be able to have a set of
        //       production bots and do 0-downtime deploys between them (with a shared DB),
        //       and run development versions without having them all respond to commands.
        // ChannelId(735953391687303260).say(&ctx, "Good morning!").unwrap();
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
                    Command::Help => {
                        if self.environment != BotEnvironment::Production {
                            return;
                        }

                        new_message
                            .channel_id
                            .send_message(&ctx, |message| {
                                self.add_help_embed(&new_message, message);
                                message
                            })
                            .unwrap();
                    }
                    Command::Link | Command::Graph => {
                        if self.environment != BotEnvironment::Production {
                            return;
                        }

                        let guild_id = new_message.guild_id.unwrap();
                        let guild_name = self.cache.get_guild(&ctx, guild_id).unwrap().name;

                        println!(
                            "\"{}:{:04}\" requested a graph for \"{}\"",
                            new_message.author.name, new_message.author.discriminator, guild_name,
                        );

                        new_message.channel_id.broadcast_typing(&ctx).unwrap();

                        let dot = {
                            let social = self.social.lock();
                            let graph = social.build_guild_graph(guild_id).unwrap();
                            graph.to_dot(&ctx, &self.cache)
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
                        let mut files = Vec::new();
                        let social = self.social.lock();
                        for &guild_id in social.get_all_guild_ids() {
                            new_message.channel_id.broadcast_typing(&ctx).unwrap();
                            println!("building guild graph for {}", guild_id);

                            let guild_name = self.cache.get_guild(&ctx, guild_id).unwrap().name;
                            let graph = social.build_guild_graph(guild_id).unwrap();
                            let dot = graph.to_dot(&ctx, &self.cache);
                            files.push((dot, format!("{}.dot", guild_name)));
                        }

                        let files: Vec<(&[u8], &str)> = files
                            .iter()
                            .map(|(contents, name)| (contents.as_bytes(), name.as_ref()))
                            .collect();

                        new_message
                            .channel_id
                            .send_files(&ctx, files, |m| m)
                            .unwrap();
                    }
                    Command::Unknown(command) => {
                        if self.environment != BotEnvironment::Production {
                            return;
                        }

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

        let message_info = self
            .cache
            .get_message(&ctx, add_reaction.channel_id, add_reaction.message_id)
            .unwrap();

        if message_info.kind != MessageType::Regular {
            return;
        }

        let interaction = Interaction::new_from_reaction(&add_reaction, &message_info).unwrap();
        self.process_interaction(&ctx, interaction);
    }
}

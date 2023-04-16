use std::process::Stdio;

use anyhow::{anyhow, Context as AnyhowContext, Result};
use futures::future::join_all;
use futures::FutureExt;
use tokio::io::AsyncWriteExt;
use tokio::process;
use tracing::{debug, error, info, warn};
use twilight_command_parser::{Arguments, CommandParserConfig, Parser};
use twilight_http::request::application::interaction::UpdateResponse;
use twilight_http::request::channel::message::CreateMessage;
use twilight_model::application::command::{
    Command, CommandOption, CommandOptionType, CommandType,
};
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::InteractionData::ApplicationCommand;
use twilight_model::channel::message::embed::{Embed, EmbedField};
use twilight_model::channel::message::{AllowedMentions, MentionType};
use twilight_model::channel::Message;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::event::Event::{GuildCreate, InteractionCreate, MessageCreate};
use twilight_model::gateway::CloseFrame;
use twilight_model::http::attachment::Attachment;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::id::marker::GuildMarker;
use twilight_model::id::Id;
use twilight_model::user::User;

use crate::context::Context;
use crate::social::graph::ColorScheme;

struct CommandContext {
    guild_id: Option<Id<GuildMarker>>,
    author: User,
}

#[derive(Debug)]
struct CommandResponse {
    content: Option<String>,
    attachments: Vec<Attachment>,
    embeds: Vec<Embed>,
}

pub async fn handle_event(context: &Context, event: &Event) -> Result<bool> {
    match event {
        GuildCreate(guild) => {
            let guild_id = guild.id;

            let commands = if Some(guild_id) == context.management_guild {
                vec![
                    Command {
                        application_id: None,
                        default_member_permissions: None,
                        dm_permission: None,
                        description: "Internal debug command.".to_string(),
                        description_localizations: None,
                        guild_id: None,
                        id: None,
                        kind: CommandType::ChatInput,
                        name: "dump".to_string(),
                        name_localizations: None,
                        nsfw: None,
                        options: vec![CommandOption {
                            autocomplete: None,
                            channel_types: None,
                            choices: None,
                            description: "ID of guild to dump.".to_string(),
                            description_localizations: None,
                            kind: CommandOptionType::String,
                            max_length: None,
                            max_value: None,
                            min_length: None,
                            min_value: None,
                            name: "guild".to_string(),
                            name_localizations: None,
                            options: None,
                            required: Some(false),
                        }],
                        version: Id::new(1),
                    },
                    Command {
                        application_id: None,
                        default_member_permissions: None,
                        dm_permission: None,
                        description: "Internal debug command.".to_string(),
                        description_localizations: None,
                        guild_id: None,
                        id: None,
                        kind: CommandType::ChatInput,
                        name: "stats".to_string(),
                        name_localizations: None,
                        nsfw: None,
                        options: Vec::new(),
                        version: Id::new(1),
                    },
                ]
            } else {
                Vec::new()
            };

            let http = context.http.clone();
            let application_id = context.application_id;

            tokio::spawn(async move {
                match http
                    .interaction(application_id)
                    .set_guild_commands(guild_id, &commands)
                    .await
                {
                    Ok(_) => debug!("setup application commands for guild {}", guild_id),
                    Err(err) => warn!(
                        "failed to setup application commands for guild {}: {:?}",
                        guild_id, err
                    ),
                }
            });

            Ok(false)
        }
        InteractionCreate(interaction) => {
            info!("interaction received: {:?}", interaction);

            let command_data = match &interaction.data {
                Some(ApplicationCommand(data)) => data,
                _ => return Ok(false),
            };

            let command_context = CommandContext {
                guild_id: interaction.guild_id,
                author: interaction
                    .user
                    .as_ref()
                    .unwrap_or_else(|| interaction.member.as_ref().unwrap().user.as_ref().unwrap())
                    .clone(),
            };

            let result_future = match command_data.name.as_str() {
                "help" => command_help(context).boxed(),
                "graph" => {
                    command_graph_from_interaction(context, &command_context, &command_data.options)
                        .boxed()
                }
                "stats" => command_stats(context).boxed(),
                "dump" => {
                    command_dump_from_interaction(context, &command_context, &command_data.options)
                        .boxed()
                }
                _ => return Ok(false),
            };

            context
                .http
                .interaction(interaction.application_id)
                .create_response(
                    interaction.id,
                    &interaction.token,
                    &InteractionResponse {
                        kind: InteractionResponseType::DeferredChannelMessageWithSource,
                        data: None,
                    },
                )
                .await?;

            let result = match result_future.await {
                Ok(response) => {
                    let response_interaction = context.http.interaction(interaction.application_id);

                    let update_response = response_interaction.update_response(&interaction.token);

                    add_command_response_to_interaction_and_send(update_response, &response).await
                }
                error => error.and(Ok(())),
            };

            if let Err(error) = result {
                error!("command failed: {:?}", error);

                context
                    .http
                    .interaction(interaction.application_id)
                    .update_response(&interaction.token)
                    .content(Some(&format!(
                        "Sorry, there was an error handling that command :warning:\n```\n{}\n```",
                        error
                    )))?
                    .await?;
            }

            Ok(true)
        }
        MessageCreate(message) => handle_message(context, message).await,
        _ => Ok(false),
    }
}

async fn add_command_response_to_interaction_and_send<'a>(
    interaction: UpdateResponse<'a>,
    response: &'a CommandResponse,
) -> Result<()> {
    let mut interaction = interaction;

    if let Some(content) = &response.content {
        interaction = interaction.content(Some(content))?;
    }

    if !response.attachments.is_empty() {
        interaction = interaction.attachments(&response.attachments)?;
    }

    if !response.embeds.is_empty() {
        interaction = interaction.embeds(Some(&response.embeds))?;
    }

    interaction.await?;

    Ok(())
}

async fn handle_message(context: &Context, message: &Message) -> Result<bool> {
    // Ignore messages from bots (including ourself)
    if message.author.bot {
        return Ok(false);
    }

    debug!("new message: {}", message.content);

    // TODO: I think we want to switch back to our own command parsing.
    let mut config = CommandParserConfig::new();
    config.add_prefix(format!("<@{}> ", context.user.id));
    config.add_prefix(format!("<@!{}> ", context.user.id));
    config.add_command("help", false);
    config.add_command("invite", false);
    config.add_command("graph", false);
    config.add_command("stats", false);
    config.add_command("dump", false);
    config.add_command("bounce", false);

    let parser = Parser::new(config);
    let command = match parser.parse(&message.content) {
        Some(command) => command,
        None => return Ok(false),
    };

    info!("received command: {:?} in message {:?}", command, message);

    let command_context = CommandContext {
        guild_id: message.guild_id,
        author: message.author.clone(),
    };

    let result = match command.name {
        "help" | "invite" => command_help(context).await,
        "graph" => command_graph_from_message(context, &command_context, command.arguments).await,
        "stats" => command_stats(context).await,
        "dump" => command_dump_from_message(context, &command_context, command.arguments).await,
        "bounce" => {
            if context.owners.contains(&command_context.author.id) {
                context.shard.close(CloseFrame::NORMAL)?;

                Ok(CommandResponse {
                    content: Some("Restarting shard...".into()),
                    attachments: vec![],
                    embeds: vec![],
                })
            } else {
                Ok(CommandResponse {
                    content: None,
                    attachments: vec![],
                    embeds: vec![],
                })
            }
        }
        _ => Err(anyhow!("unknown command")),
    };

    let allowed_mentions = AllowedMentions {
        parse: vec![MentionType::Users],
        replied_user: false,
        roles: vec![],
        users: vec![],
    };

    let result = match result {
        Ok(response) => {
            let response_message = context
                .http
                .create_message(message.channel_id)
                .reply(message.id)
                .allowed_mentions(Some(&allowed_mentions));

            add_command_response_to_message_and_send(response_message, &response).await
        }
        error => error.and(Ok(())),
    };

    if let Err(error) = result {
        error!("command failed: {:?}", error);

        context
            .http
            .create_message(message.channel_id)
            .reply(message.id)
            .allowed_mentions(Some(&allowed_mentions))
            .content(&format!(
                "Sorry, there was an error handling that command :warning:\n```\n{}\n```",
                error
            ))?
            .await?;
    }

    Ok(true)
}

async fn add_command_response_to_message_and_send<'a>(
    message: CreateMessage<'a>,
    response: &'a CommandResponse,
) -> Result<()> {
    let mut message = message;

    if let Some(content) = &response.content {
        message = message.content(content)?;
    }

    if !response.attachments.is_empty() {
        message = message.attachments(&response.attachments)?;
    }

    if !response.embeds.is_empty() {
        message = message.embeds(&response.embeds)?;
    }

    message.await?;

    Ok(())
}

async fn command_help(context: &Context) -> Result<CommandResponse> {
    let description = format!(
        "I'm a Discord Bot that infers relationships between users and draws pretty graphs.\n\
        I'll only respond to messages that directly mention me, like `@{} help`,\n\
        or you can use my slash commands.",
        context.user.name,
    );

    let commands_field = EmbedField {
        inline: false,
        name: "Commands".to_string(),
        value: vec![
            "` help               `\u{2000}This message.",
            "` graph [light|dark] `\u{2000}Get a preview-quality graph image.",
        ]
        .join("\n"),
    };

    let invite_url = format!(
        "https://discord.com/api/oauth2/authorize?client_id={}&permissions=277025508416&scope=bot",
        context.user.id,
    );

    let invite_field = EmbedField {
        inline: false,
        name: "Want graphs for your guild?".to_string(),
        value: format!(
            "[Click here]({}) to invite the bot to join your server.",
            invite_url,
        ),
    };

    let embed = Embed {
        author: None,
        color: None,
        description: Some(description),
        fields: vec![commands_field, invite_field],
        footer: None,
        image: None,
        kind: "rich".to_string(),
        provider: None,
        thumbnail: None,
        timestamp: None,
        title: None,
        url: None,
        video: None,
    };

    Ok(CommandResponse {
        content: None,
        attachments: vec![],
        embeds: vec![embed],
    })
}

async fn command_graph_from_message(
    context: &Context,
    command: &CommandContext,
    mut arguments: Arguments<'_>,
) -> Result<CommandResponse> {
    let color_scheme = match arguments.next() {
        Some("light") => ColorScheme::Light,
        Some("dark") => ColorScheme::Dark,
        Some(value) => anyhow::bail!(
            "{} is not a recognized color scheme, expected \"light\" or \"dark\"",
            value,
        ),
        None => ColorScheme::Dark,
    };

    let transparent = matches!(arguments.next(), Some("transparent"));

    command_graph(context, command, color_scheme, transparent).await
}

async fn command_graph_from_interaction(
    context: &Context,
    command: &CommandContext,
    options: &[CommandDataOption],
) -> Result<CommandResponse> {
    let (color_scheme, transparent) = if let Some(CommandDataOption {
        value: CommandOptionValue::String(style),
        ..
    }) = options.iter().find(|i| i.name == "style")
    {
        match style.as_str() {
            "light" => (ColorScheme::Light, false),
            "dark" => (ColorScheme::Dark, false),
            "transparent light" => (ColorScheme::Light, true),
            "transparent dark" => (ColorScheme::Dark, true),
            _ => anyhow::bail!("{} is not a recognized graph style", style),
        }
    } else {
        (ColorScheme::Dark, false)
    };

    command_graph(context, command, color_scheme, transparent).await
}

async fn command_graph(
    context: &Context,
    command: &CommandContext,
    color_scheme: ColorScheme,
    transparent: bool,
) -> Result<CommandResponse> {
    let guild_id = command.guild_id.context("message not to guild")?;
    let guild_name = context.cache.get_guild(guild_id).await?.name;
    let attachment_base_name = sanitize_name_for_attachment(&guild_name);

    let graph = {
        let social = context.social.lock();

        social
            .build_guild_graph(guild_id)
            .context("no graph for guild")?
    };

    let dot = graph
        .to_dot(
            context,
            guild_id,
            Some(&command.author),
            color_scheme,
            transparent,
            &context.font_name,
        )
        .await?;

    let png = render_dot(&dot).await?;

    let png = if transparent {
        add_png_shadow(&png, color_scheme).await?
    } else {
        png
    };

    Ok(CommandResponse {
        content: None,
        attachments: vec![Attachment::from_bytes(
            attachment_base_name + ".png",
            png,
            0,
        )],
        embeds: vec![],
    })
}

async fn command_stats(context: &Context) -> Result<CommandResponse> {
    Ok(CommandResponse {
        content: Some(format!("{:?}", context.cache.get_stats())),
        attachments: vec![],
        embeds: vec![],
    })
}

async fn command_dump_from_message(
    context: &Context,
    command: &CommandContext,
    mut arguments: Arguments<'_>,
) -> Result<CommandResponse> {
    if !context.owners.contains(&command.author.id) {
        info!(
            "{} tried to run dump command but isn't an owner",
            command.author.id,
        );

        // TODO: Fail silently.
        return Ok(CommandResponse {
            content: None,
            attachments: vec![],
            embeds: vec![],
        });
    }

    if let Some(guild_id) = arguments.next() {
        let guild_id: u64 = guild_id.parse()?;
        let guild_id = Id::new(guild_id);

        command_dump_with_guild(context, guild_id).await
    } else {
        command_dump_without_guild(context).await
    }
}

async fn command_dump_from_interaction(
    context: &Context,
    command: &CommandContext,
    options: &[CommandDataOption],
) -> Result<CommandResponse> {
    if !context.owners.contains(&command.author.id) {
        info!(
            "{} tried to run dump command but isn't an owner",
            command.author.id,
        );

        // TODO: Fail silently.
        return Ok(CommandResponse {
            content: None,
            attachments: vec![],
            embeds: vec![],
        });
    }

    if let Some(CommandDataOption {
        value: CommandOptionValue::String(guild_id),
        ..
    }) = options.iter().find(|i| i.name == "guild")
    {
        let guild_id: u64 = guild_id.parse()?;
        let guild_id = Id::new(guild_id);

        command_dump_with_guild(context, guild_id).await
    } else {
        command_dump_without_guild(context).await
    }
}

async fn command_dump_with_guild(
    context: &Context,
    guild_id: Id<GuildMarker>,
) -> Result<CommandResponse> {
    let guild_name = context.cache.get_guild(guild_id).await?.name;
    let attachment_base_name = sanitize_name_for_attachment(&guild_name);

    let graph = {
        let social = context.social.lock();

        social
            .build_guild_graph(guild_id)
            .context("no graph for guild")?
    };

    let dot = graph
        .to_dot(
            context,
            guild_id,
            None,
            ColorScheme::Light,
            false,
            &context.font_name,
        )
        .await?;

    let png = render_dot(&dot).await?;

    Ok(CommandResponse {
        content: None,
        attachments: vec![
            Attachment::from_bytes(attachment_base_name.clone() + ".dot", dot.into_bytes(), 0),
            Attachment::from_bytes(attachment_base_name + ".png", png, 1),
        ],
        embeds: vec![],
    })
}

async fn command_dump_without_guild(context: &Context) -> Result<CommandResponse> {
    let guild_ids = {
        let social = context.social.lock();
        social.get_all_guild_ids()
    };

    let guild_futures = guild_ids
        .into_iter()
        .take(20)
        .map(|(guild_id, _)| context.cache.get_guild(guild_id));

    let guilds: Vec<_> = join_all(guild_futures)
        .await
        .into_iter()
        .filter_map(|guild| guild.ok())
        .map(|guild| format!("{} - {}", guild.id, guild.name))
        .collect();

    let mut content = "Largest 20 guilds:\n".to_owned();
    content.push_str(&guilds.join("\n"));

    Ok(CommandResponse {
        content: Some(content),
        attachments: vec![],
        embeds: vec![],
    })
}

fn sanitize_name_for_attachment(name: &str) -> String {
    let mut string = String::with_capacity(name.len());
    let mut prev_escaped = false;

    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
            string.push(c);
            prev_escaped = false;
        } else if !prev_escaped {
            string.push('_');
            prev_escaped = true;
        }
    }

    string
}

async fn render_dot(dot: &str) -> Result<Vec<u8>> {
    let mut graphviz = process::Command::new("dot")
        .arg("-v")
        .arg("-Tpng")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    {
        let stdin = graphviz.stdin.as_mut().unwrap();
        stdin.write_all(dot.as_bytes()).await?;
    }

    let output = graphviz.wait_with_output().await?;

    if !output.status.success() {
        anyhow::bail!("graphviz failed");
    }

    Ok(output.stdout)
}

async fn add_png_shadow(input: &[u8], color_scheme: ColorScheme) -> Result<Vec<u8>> {
    let background_color = match color_scheme {
        ColorScheme::Light => 0xFFFFFF,
        ColorScheme::Dark => 0x313338,
    };

    let mut convert = process::Command::new("convert")
        .arg("png:-")
        .arg("-background")
        .arg("none")
        .arg("(")
        .arg("+clone")
        .arg("-background")
        .arg(format!("#{:06X}", background_color))
        .arg("-shadow")
        .arg("200x2+0+0")
        .arg(")")
        .arg("-background")
        .arg("none")
        .arg("-compose")
        .arg("DstOver")
        .arg("-flatten")
        .arg("png:-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    {
        let stdin = convert.stdin.as_mut().unwrap();
        stdin.write_all(input).await?;
    }

    let output = convert.wait_with_output().await?;

    if !output.status.success() {
        anyhow::bail!("convert failed");
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::sanitize_name_for_attachment;

    #[test]
    fn test_sanitize_name_for_attachment() {
        assert_eq!(
            sanitize_name_for_attachment("Name_ With & Spaces"),
            "Name_With_Spaces"
        );
    }
}

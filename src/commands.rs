use anyhow::{Context as AnyhowContext, Result};
use futures::future::join_all;
use tokio::io::AsyncWriteExt;
use tokio::process;
use tracing::{debug, error, info};
use twilight_command_parser::{Arguments, CommandParserConfig, Parser};
use twilight_model::channel::message::embed::{Embed, EmbedField, EmbedFooter};
use twilight_model::channel::Message;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::event::Event::MessageCreate;
use twilight_model::id::Id;

use std::process::Stdio;
use twilight_model::http::attachment::Attachment;

use crate::context::Context;
use crate::social::graph::ColorScheme;

pub async fn handle_event(context: &Context, event: &Event) -> Result<bool> {
    match event {
        MessageCreate(message) => handle_message(context, message).await,
        _ => Ok(false),
    }
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

    let parser = Parser::new(config);
    let command = match parser.parse(&message.content) {
        Some(command) => command,
        None => return Ok(false),
    };

    info!("received command: {:?} in message {:?}", command, message);

    let result = match command.name {
        "help" | "invite" => command_help(context, message).await,
        "graph" => command_graph(context, message, command.arguments).await,
        "stats" => command_stats(context, message).await,
        "dump" => command_dump(context, message, command.arguments).await,
        _ => Ok(()),
    };

    if let Err(error) = result {
        error!("command failed: {:?}", error);

        context
            .http
            .create_message(message.channel_id)
            .content(&format!(
                "Sorry, there was an error handling that command :warning:\n```\n{}\n```",
                error
            ))?
            .await?;
    }

    Ok(true)
}

async fn command_help(context: &Context, message: &Message) -> Result<()> {
    let description = format!(
        "I'm a Discord Bot that infers relationships between users and draws pretty graphs.\n\
        I'll only respond to messages that directly mention me, like `@{} help`.",
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
        "https://discord.com/api/oauth2/authorize?client_id={}&permissions=117824&scope=bot",
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

    let footer = EmbedFooter {
        icon_url: None,
        proxy_icon_url: None,
        text: format!(
            "Sent in response to a command from {}#{:04}",
            message.author.name, message.author.discriminator,
        ),
    };

    let embed = Embed {
        author: None,
        color: None,
        description: Some(description),
        fields: vec![commands_field, invite_field],
        footer: Some(footer),
        image: None,
        kind: "rich".to_string(),
        provider: None,
        thumbnail: None,
        timestamp: None,
        title: None,
        url: None,
        video: None,
    };

    context
        .http
        .create_message(message.channel_id)
        .embeds(&[embed])?
        .await?;

    Ok(())
}

async fn command_graph(
    context: &Context,
    message: &Message,
    mut arguments: Arguments<'_>,
) -> Result<()> {
    // TODO: Respond to the command on errors.

    let guild_id = message.guild_id.context("message not to guild")?;
    let guild_name = context.cache.get_guild(guild_id).await?.name;
    let attachment_base_name = sanitize_name_for_attachment(&guild_name);

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
            Some(&message.author),
            color_scheme,
            transparent,
        )
        .await?;

    let png = render_dot(&dot).await?;

    let png = if transparent {
        add_png_shadow(&png, color_scheme).await?
    } else {
        png
    };

    context
        .http
        .create_message(message.channel_id)
        .attachments(&[Attachment::from_bytes(
            attachment_base_name + ".png",
            png,
            0,
        )])?
        .await?;

    Ok(())
}

async fn command_stats(context: &Context, message: &Message) -> Result<()> {
    context
        .http
        .create_message(message.channel_id)
        .content(&format!("{:?}", context.cache.get_stats()))?
        .await?;

    Ok(())
}

async fn command_dump(
    context: &Context,
    message: &Message,
    mut arguments: Arguments<'_>,
) -> Result<()> {
    if !context.owners.contains(&message.author.id) {
        info!(
            "{} tried to run dump command but isn't an owner",
            message.author.id,
        );
        return Ok(());
    }

    if let Some(guild_id) = arguments.next() {
        let guild_id: u64 = guild_id.parse()?;
        let guild_id = Id::new(guild_id);

        let guild_name = context.cache.get_guild(guild_id).await?.name;
        let attachment_base_name = sanitize_name_for_attachment(&guild_name);

        let graph = {
            let social = context.social.lock();

            social
                .build_guild_graph(guild_id)
                .context("no graph for guild")?
        };

        let dot = graph
            .to_dot(context, guild_id, None, ColorScheme::Light, false)
            .await?;

        let png = render_dot(&dot).await?;

        context
            .http
            .create_message(message.channel_id)
            .attachments(&[
                Attachment::from_bytes(attachment_base_name.clone() + ".dot", dot.into_bytes(), 0),
                Attachment::from_bytes(attachment_base_name + ".png", png, 1),
            ])?
            .await?;

        return Ok(());
    }

    let guild_ids = {
        let social = context.social.lock();
        social.get_all_guild_ids()
    };

    let guild_futures = guild_ids
        .into_iter()
        .map(|guild_id| context.cache.get_guild(guild_id));

    let guilds: Vec<_> = join_all(guild_futures)
        .await
        .into_iter()
        .filter_map(|guild| guild.ok())
        .map(|guild| format!("{} - {}", guild.id, guild.name))
        .collect();

    let mut content = "Guilds:\n".to_owned();
    content.push_str(&guilds.join("\n"));

    context
        .http
        .create_message(message.channel_id)
        .content(&content)?
        .await?;

    Ok(())
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

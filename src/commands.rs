use anyhow::Result;
use tracing::info;
use twilight_command_parser::{Command, CommandParserConfig, Parser};
use twilight_model::channel::embed::{Embed, EmbedField, EmbedFooter};
use twilight_model::channel::Message;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::event::Event::MessageCreate;

use crate::context::Context;

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

    info!("new message: {}", message.content);

    // TODO: I think we want to switch back to our own command parsing.
    let mut config = CommandParserConfig::new();
    config.add_prefix(format!("<@{}>", context.user.id));
    config.add_prefix(format!("<@!{}>", context.user.id));
    config.add_command("help", false);
    config.add_command("graph", false);
    config.add_command("stats", false);

    let parser = Parser::new(config);
    let command = match parser.parse(&message.content) {
        Some(command) => command,
        None => return Ok(false),
    };

    info!("command: {:?}", command);

    match command {
        Command { name: "help", .. } => command_help(context, message).await?,
        Command { name: "graph", .. } => todo!(),
        Command { name: "stats", .. } => command_stats(context, message).await?,
        _ => (),
    };

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
            "` help  `\u{2000}This message.",
            "` graph `\u{2000}Get a preview-quality graph image.",
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
        .embed(embed)?
        .await?;

    Ok(())
}

async fn command_stats(context: &Context, message: &Message) -> Result<()> {
    context
        .http
        .create_message(message.channel_id)
        .content(format!(
            "<@{}> {:?}",
            message.author.id,
            context.cache.get_stats()
        ))?
        .await?;

    Ok(())
}

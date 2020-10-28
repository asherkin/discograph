use anyhow::Result;
use tracing::info;
use twilight_command_parser::{Command, CommandParserConfig, Parser};
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

    let user_mention_prefix = format!("<@{}>", context.user.id);
    let nickname_mention_prefix = format!("<@!{}>", context.user.id);

    let mut config = CommandParserConfig::new();
    config.add_command("help", false);
    config.add_command("graph", false);

    let parser = Parser::new(config);

    let command = parser
        .parse_with_prefix(&nickname_mention_prefix, &message.content)
        .or_else(|| parser.parse_with_prefix(&user_mention_prefix, &message.content));

    let command = match command {
        Some(command) => command,
        None => return Ok(false),
    };

    info!("command: {:?}", command);

    match command {
        Command { name: "help", .. } => command_help(context, message).await?,
        Command { name: "graph", .. } => todo!(),
        _ => (),
    };

    Ok(true)
}

async fn command_help(context: &Context, message: &Message) -> Result<()> {
    context
        .http
        .create_message(message.channel_id)
        .content("This is a help message.")?
        .await?;

    Ok(())
}

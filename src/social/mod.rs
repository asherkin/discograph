pub mod graph;
pub mod inference;

use anyhow::Result;
use std::collections::HashSet;
use tracing::{error, info};
use twilight_model::channel::message::{MessageReference, MessageType};
use twilight_model::channel::ChannelType;
use twilight_model::gateway::event::Event;

use crate::context::Context;
use crate::social::inference::{Interaction, RelationshipChange};
use crate::stats;

pub async fn handle_event(context: &Context, event: &Event) -> Result<()> {
    match event {
        Event::GuildCreate(guild) => {
            // Load any existing graphs into memory for the guild's channels.
            let mut social = context.social.lock();
            for channel in &guild.channels {
                social.get_graph(guild.id, channel.id);
            }
        }
        Event::GuildDelete(guild) => {
            let mut social = context.social.lock();
            social.remove_guild(guild.id);
        }
        Event::ChannelCreate(channel) if channel.kind == ChannelType::GuildText => {
            if let Some(guild_id) = channel.guild_id {
                // Load any existing graph into memory for the channel.
                let mut social = context.social.lock();
                social.get_graph(guild_id, channel.id);
            }
        }
        Event::ChannelDelete(channel) => {
            if let Some(guild_id) = channel.guild_id {
                let mut social = context.social.lock();
                social.remove_channel(guild_id, channel.id);
            }
        }
        Event::MessageCreate(message)
            if (message.kind == MessageType::Regular || message.kind == MessageType::Reply)
                && message.author.id != context.user.id =>
        {
            let referenced_message = match message.reference {
                Some(MessageReference {
                    guild_id,
                    channel_id: Some(channel_id),
                    message_id: Some(message_id),
                    ..
                }) => Some(
                    context
                        .cache
                        .get_message(guild_id, channel_id, message_id)
                        .await?,
                ),
                _ => None,
            };

            let interaction = Interaction::new_from_message(message, referenced_message.as_ref())?;
            process_interaction(context, interaction).await;
        }
        Event::ReactionAdd(reaction) if reaction.user_id != context.user.id => {
            let message = context
                .cache
                .get_message(reaction.guild_id, reaction.channel_id, reaction.message_id)
                .await?;

            let interaction = Interaction::new_from_reaction(reaction, &message)?;
            process_interaction(context, interaction).await;
        }
        _ => (),
    }

    Ok(())
}

async fn process_interaction(context: &Context, interaction: Interaction) {
    // Calculate this first, before we do anything async.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let interaction_string = interaction.to_string(&context.cache).await;
    let mut debug_lines = vec![interaction_string];

    let changes = {
        let mut social = context.social.lock();

        let changes = social.infer(&interaction);
        for change in &changes {
            debug_lines.push(format!("| {}", change));
        }

        social.apply(&interaction, &changes);

        changes
    };

    for line in &debug_lines {
        info!("{}", line);
    }

    let debug_enabled = {
        context
            .channels_with_debug_enabled
            .lock()
            .contains(&interaction.channel)
    };

    if debug_enabled {
        let result = context
            .http
            .create_message(interaction.channel)
            .content(&debug_lines.join("\n"))
            .unwrap()
            .await;

        if let Err(error) = result {
            error!(?error, "failed to send debug message");
        }
    }

    if let Err(error) = store_interaction(context, interaction, timestamp, changes).await {
        error!(?error, "failed to store interaction");
    }
}

pub async fn store_interaction(
    context: &Context,
    interaction: Interaction,
    timestamp: u64,
    changes: Vec<RelationshipChange>,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }

    let pool = match &context.pool {
        Some(pool) => pool,
        None => return Ok(()),
    };

    let mut values = "(?, ?, ?, ?, ?, ?), ".repeat(changes.len());
    values.truncate(values.len() - 2);

    let sql = format!(
        "INSERT INTO events (timestamp, guild, channel, source, target, reason) VALUES {}",
        values
    );

    let mut user_ids = HashSet::new();
    let mut query = sqlx::query(&sql);

    for change in changes {
        user_ids.insert(change.source);
        user_ids.insert(change.target);

        query = query
            .bind(timestamp)
            .bind(interaction.guild.get())
            .bind(interaction.channel.get())
            .bind(change.source.get())
            .bind(change.target.get())
            .bind(change.reason as u8);
    }

    // Ensure the DB contains the details of who was involved in this interaction.
    stats::ensure_users_saved_in_db(
        context.cache.clone(),
        pool,
        &context.shard,
        interaction.guild,
        user_ids.into_iter(),
    )
    .await?;

    // Once the related records are in, actually insert the event.
    query.execute(pool).await?;

    Ok(())
}

use anyhow::Result;
use sqlx::MySqlPool;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::event::Event::{GuildCreate, GuildDelete, Ready};
use twilight_model::id::marker::GuildMarker;
use twilight_model::id::Id;

use crate::context::Context;

pub async fn handle_event(context: &Context, event: &Event) -> Result<()> {
    let pool = match &context.pool {
        Some(pool) => pool,
        None => return Ok(()),
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    match event {
        Ready(ready) => {
            for guild in &ready.guilds {
                sqlx::query("INSERT INTO guilds (id, joined) VALUES (?, ?) ON DUPLICATE KEY UPDATE departed = NULL, online = 0")
                    .bind(guild.id.get())
                    .bind(timestamp)
                    .execute(pool)
                    .await?;
            }
        }
        GuildCreate(guild) => {
            let joined_at = guild
                .joined_at
                .map_or(timestamp, |d| (d.as_micros() / 1000) as u64);

            sqlx::query("INSERT INTO guilds (id, joined, online) VALUES (?, ?, 1) ON DUPLICATE KEY UPDATE joined = IF(joined < VALUE(joined), joined, VALUE(joined)), departed = NULL, online = 1")
                .bind(guild.id.get())
                .bind(joined_at)
                .execute(pool)
                .await?;
        }
        GuildDelete(guild) => {
            if guild.unavailable {
                sqlx::query("UPDATE guilds SET online = 0 WHERE id = ?")
                    .bind(guild.id.get())
                    .execute(pool)
                    .await?;
            } else {
                sqlx::query("UPDATE guilds SET departed = ?, online = 0 WHERE id = ?")
                    .bind(timestamp)
                    .bind(guild.id.get())
                    .execute(pool)
                    .await?;
            }
        }
        _ => (),
    }

    Ok(())
}

pub enum CommandType {
    Chat,
    Slash,
}

pub async fn record_graph_command(
    context: &Context,
    guild_id: Id<GuildMarker>,
    source: CommandType,
) -> Result<()> {
    let pool = match &context.pool {
        Some(pool) => pool,
        None => return Ok(()),
    };

    match source {
        CommandType::Chat => {
            sqlx::query("UPDATE guilds SET chat_graphs = chat_graphs + 1 WHERE id = ?")
                .bind(guild_id.get())
                .execute(pool)
                .await?;
        }
        CommandType::Slash => {
            sqlx::query("UPDATE guilds SET slash_graphs = slash_graphs + 1 WHERE id = ?")
                .bind(guild_id.get())
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}

pub async fn reset_guilds(pool: &MySqlPool) -> Result<()> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // We set the departed timestamp to now in case we were removed from any servers while offline.
    // The Ready event will clear this for any guilds we're still joined in a moment.
    sqlx::query("UPDATE guilds SET departed = COALESCE(departed, ?), online = 0")
        .bind(timestamp)
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn set_offline(pool: &MySqlPool) -> Result<()> {
    sqlx::query("UPDATE guilds SET online = 0")
        .execute(pool)
        .await?;

    Ok(())
}

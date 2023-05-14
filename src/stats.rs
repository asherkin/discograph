use anyhow::Result;
use futures::future::join_all;
use sqlx::{MySqlPool, Row};
use twilight_gateway::MessageSender;
use twilight_model::gateway::event::Event;
use twilight_model::guild::Member;
use twilight_model::id::marker::{GuildMarker, UserMarker};
use twilight_model::id::Id;

use std::sync::Arc;

use crate::cache::{Cache, CachedMember};
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
        Event::Ready(ready) => {
            for guilds in ready.guilds.chunks(10_000) {
                let mut values = "(?, ?), ".repeat(guilds.len());
                values.truncate(values.len() - 2);

                let sql = format!("INSERT INTO guilds (id, joined) VALUES {} ON DUPLICATE KEY UPDATE departed = NULL, online = 0", values);

                let mut query = sqlx::query(&sql);
                for guild in guilds {
                    query = query.bind(guild.id.get()).bind(timestamp);
                }

                query.execute(pool).await?;
            }
        }
        Event::GuildCreate(guild) => {
            let joined_at = guild
                .joined_at
                .map_or(timestamp, |d| (d.as_micros() / 1000) as u64);

            sqlx::query("INSERT INTO guilds (id, joined, online) VALUES (?, ?, 1) ON DUPLICATE KEY UPDATE joined = IF(joined < VALUE(joined), joined, VALUE(joined)), departed = NULL, online = 1")
                .bind(guild.id.get())
                .bind(joined_at)
                .execute(pool)
                .await?;

            // Clear out all the member info, users might have joined or left while we were offline.
            // If someone had joined, left, then joined again while we were offline, we leave them
            // marked as departed to avoid making invalid requests to Discord. We'll pick them up
            // when they next interact.
            sqlx::query("DELETE FROM members WHERE guild = ? AND departed = 0")
                .bind(guild.id.get())
                .execute(pool)
                .await?;

            // Asynchronously preload the information for members we've seen recent activity from.
            // TODO: It's quite likely we want to just let the web UI drive this via API, as it'll need bulk loading anyway.
            let cache = context.cache.clone();
            let shard = context.shard.clone();
            let pool = pool.clone();
            let guild_id = guild.id;

            tokio::spawn(async move {
                let members = sqlx::query("(SELECT source AS user FROM events WHERE guild = ? ORDER BY timestamp DESC LIMIT 5000) UNION DISTINCT (SELECT target AS user FROM events WHERE guild = ? ORDER BY timestamp DESC LIMIT 5000)")
                    .bind(guild_id.get())
                    .bind(guild_id.get())
                    .try_map(|row| {
                        Id::new_checked(row.get(0))
                            .ok_or(sqlx::Error::ColumnDecode {
                                index: "0".into(),
                                source: "invalid id".into(),
                            })
                    })
                    .fetch_all(&pool)
                    .await
                    .unwrap();

                // The Event::MemberChunk handler below does the actual work.
                cache
                    .bulk_request_members(&shard, guild_id, &members)
                    .await
                    .unwrap();
            });
        }
        Event::GuildDelete(guild) => {
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
        Event::MemberAdd(member) => {
            // We only update their information if we already have them in the DB, to avoid it
            // growing with data that isn't of any interest.

            let result = sqlx::query("UPDATE users SET name = ?, discriminator = ?, bot = ?, avatar = ?, animated = ? WHERE id = ?")
                .bind(&member.user.name)
                .bind(member.user.discriminator)
                .bind(member.user.bot)
                .bind(member.user.avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(member.user.avatar.map_or(false, |image| image.is_animated()))
                .bind(member.user.id.get())
                .execute(pool)
                .await?;

            if result.rows_affected() > 0 {
                sqlx::query("INSERT INTO members (guild, user, nickname, avatar, animated) VALUES (?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE nickname = VALUE(nickname), avatar = VALUE(avatar), animated = VALUE(animated), departed = 0")
                    .bind(member.guild_id.get())
                    .bind(member.user.id.get())
                    .bind(&member.nick)
                    .bind(member.avatar.map(|image| image.bytes().as_slice().to_owned()))
                    .bind(member.avatar.map_or(false, |image| image.is_animated()))
                    .execute(pool)
                    .await?;
            }
        }
        Event::MemberUpdate(member) => {
            // We only update their information if we already have them in the DB, to avoid it
            // growing with data that isn't of any interest.

            let result = sqlx::query("UPDATE users SET name = ?, discriminator = ?, bot = ?, avatar = ?, animated = ? WHERE id = ?")
                .bind(&member.user.name)
                .bind(member.user.discriminator)
                .bind(member.user.bot)
                .bind(member.user.avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(member.user.avatar.map_or(false, |image| image.is_animated()))
                .bind(member.user.id.get())
                .execute(pool)
                .await?;

            if result.rows_affected() > 0 {
                sqlx::query("INSERT INTO members (guild, user, nickname, avatar, animated) VALUES (?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE nickname = VALUE(nickname), avatar = VALUE(avatar), animated = VALUE(animated), departed = 0")
                    .bind(member.guild_id.get())
                    .bind(member.user.id.get())
                    .bind(&member.nick)
                    .bind(member.avatar.map(|image| image.bytes().as_slice().to_owned()))
                    .bind(member.avatar.map_or(false, |image| image.is_animated()))
                    .execute(pool)
                    .await?;
            }
        }
        Event::MemberRemove(member) => {
            sqlx::query("UPDATE users SET name = ?, discriminator = ?, bot = ?, avatar = ?, animated = ? WHERE id = ?")
                .bind(&member.user.name)
                .bind(member.user.discriminator)
                .bind(member.user.bot)
                .bind(member.user.avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(member.user.avatar.map_or(false, |image| image.is_animated()))
                .bind(member.user.id.get())
                .execute(pool)
                .await?;

            sqlx::query("UPDATE members SET nickname = NULL, avatar = NULL, animated = 0, departed = 1 WHERE guild = ? AND user = ?")
                .bind(member.guild_id.get())
                .bind(member.user.id.get())
                .execute(pool)
                .await?;
        }
        Event::MemberChunk(chunk) => {
            let pool = pool.clone();
            let guild_id = chunk.guild_id;
            let members = chunk.members.clone();
            let not_found = chunk.not_found.clone();

            tokio::spawn(async move {
                store_members(&pool, guild_id, &members).await.unwrap();

                if !not_found.is_empty() {
                    let mut values = "(?, ?, 1), ".repeat(not_found.len());
                    values.truncate(values.len() - 2);

                    let sql = format!("INSERT INTO members (guild, user, departed) VALUES {} ON DUPLICATE KEY UPDATE nickname = NULL, avatar = NULL, animated = 0, departed = 1", values);

                    let mut query = sqlx::query(&sql);
                    for user_id in not_found {
                        query = query.bind(guild_id.get()).bind(user_id.get());
                    }

                    query.execute(&pool).await.unwrap();
                }
            });
        }
        _ => (),
    }

    Ok(())
}

async fn store_members(
    pool: &MySqlPool,
    guild_id: Id<GuildMarker>,
    members: &[Member],
) -> Result<()> {
    if members.is_empty() {
        return Ok(());
    }

    {
        let mut values = "(?, ?, ?, ?, ?, ?), ".repeat(members.len());
        values.truncate(values.len() - 2);

        let sql = format!("INSERT INTO users (id, name, discriminator, bot, avatar, animated) VALUES {} ON DUPLICATE KEY UPDATE name = VALUE(name), discriminator = VALUE(discriminator), bot = VALUE(bot), avatar = VALUE(avatar), animated = VALUE(animated)", values);

        let mut query = sqlx::query(&sql);
        for member in members {
            let avatar = &member.user.avatar;
            query = query
                .bind(member.user.id.get())
                .bind(&member.user.name)
                .bind(member.user.discriminator)
                .bind(member.user.bot)
                .bind(avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(avatar.map_or(false, |image| image.is_animated()));
        }

        query.execute(pool).await?;
    }

    {
        let mut values = "(?, ?, ?, ?, ?), ".repeat(members.len());
        values.truncate(values.len() - 2);

        let sql = format!("INSERT INTO members (guild, user, nickname, avatar, animated) VALUES {} ON DUPLICATE KEY UPDATE nickname = VALUE(nickname), avatar = VALUE(avatar), animated = VALUE(animated)", values);

        let mut query = sqlx::query(&sql);
        for member in members {
            let avatar = &member.avatar;
            query = query
                .bind(guild_id.get())
                .bind(member.user.id.get())
                .bind(&member.nick)
                .bind(avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(avatar.map_or(false, |image| image.is_animated()));
        }

        query.execute(pool).await?;
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

pub async fn ensure_users_saved_in_db(
    cache: Arc<Cache>,
    pool: &MySqlPool,
    shard: &MessageSender,
    guild_id: Id<GuildMarker>,
    user_ids: impl Iterator<Item = Id<UserMarker>>,
) -> Result<()> {
    let (already_loaded, not_found) = cache
        .bulk_preload_members(shard, guild_id, user_ids)
        .await
        .unwrap();

    // Users not already in the cache will have been saved by the gateway event handler.
    let already_loaded = already_loaded.into_iter().map(|user_id| {
        let cache = cache.clone();

        async move {
            let member = cache.get_member(guild_id, user_id).await;
            let user = cache.get_user(user_id).await;
            (user, member.map(|member| (user_id, member)))
        }
    });

    // Not found users are no longer members (or are webhooks), but we still need their user info.
    // For webhooks this'll pull their latest profile info from the cache, which is the best we can do.
    // Like before, the gateway event handler will have added their departed member records.
    let not_found = not_found.into_iter().map(|user_id| {
        let cache = cache.clone();

        async move {
            let user = cache.get_user(user_id).await;
            (
                user,
                Result::<(Id<UserMarker>, CachedMember), _>::Err(anyhow::anyhow!(
                    "departed member"
                )),
            )
        }
    });

    let (users, members): (Vec<_>, Vec<_>) = join_all(already_loaded)
        .await
        .into_iter()
        .chain(join_all(not_found).await.into_iter())
        .unzip();

    let users: Vec<_> = users.into_iter().flatten().collect();
    if !users.is_empty() {
        let mut values = "(?, ?, ?, ?, ?, ?), ".repeat(users.len());
        values.truncate(values.len() - 2);

        let sql = format!("INSERT INTO users (id, name, discriminator, bot, avatar, animated) VALUES {} ON DUPLICATE KEY UPDATE name = VALUE(name), discriminator = VALUE(discriminator), bot = VALUE(bot), avatar = VALUE(avatar), animated = VALUE(animated)", values);

        let mut query = sqlx::query(&sql);
        for user in &users {
            let avatar = &user.avatar;
            query = query
                .bind(user.id.get())
                .bind(&user.name)
                .bind(user.discriminator)
                .bind(user.bot)
                .bind(avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(avatar.map_or(false, |image| image.is_animated()));
        }

        query.execute(pool).await?;
    }

    let members: Vec<_> = members.into_iter().flatten().collect();
    if !members.is_empty() {
        let mut values = "(?, ?, ?, ?, ?), ".repeat(members.len());
        values.truncate(values.len() - 2);

        let sql = format!("INSERT INTO members (guild, user, nickname, avatar, animated) VALUES {} ON DUPLICATE KEY UPDATE nickname = VALUE(nickname), avatar = VALUE(avatar), animated = VALUE(animated)", values);

        let mut query = sqlx::query(&sql);
        for (user_id, member) in &members {
            let avatar = &member.avatar;
            query = query
                .bind(guild_id.get())
                .bind(user_id.get())
                .bind(&member.nick)
                .bind(avatar.map(|image| image.bytes().as_slice().to_owned()))
                .bind(avatar.map_or(false, |image| image.is_animated()));
        }

        query.execute(pool).await?;
    }

    Ok(())
}

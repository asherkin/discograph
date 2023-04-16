mod cache;
mod commands;
mod context;
mod social;

use anyhow::{Context as AnyhowContext, Result};
use futures::StreamExt;
use parking_lot::Mutex;
use sqlx::mysql::MySqlPoolOptions;
use sqlx::Connection;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use twilight_gateway::stream::ShardEventStream;
use twilight_gateway::{stream, Config, Event};
use twilight_http::{Client as HttpClient, Client};
use twilight_model::application::command::{
    Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue, CommandOptionType,
    CommandType,
};
use twilight_model::gateway::payload::outgoing::UpdatePresence;
use twilight_model::gateway::presence::{Activity, ActivityType, MinimalActivity, Status};
use twilight_model::gateway::{CloseFrame, Intents, ShardId};
use twilight_model::id::marker::{ApplicationMarker, GuildMarker, UserMarker};
use twilight_model::id::Id;
use twilight_model::oauth::team::TeamMembershipState;

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::cache::Cache;
use crate::context::Context;
use crate::social::graph::SocialGraph;

fn get_optional_env(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => panic!("{}", error),
    }
}

struct LastPresenceUpdate {
    guild_counts: HashMap<ShardId, usize>,
    when: Instant,
    count: usize,
}

impl LastPresenceUpdate {
    fn new() -> Self {
        Self {
            guild_counts: HashMap::new(),
            when: Instant::now(),
            count: 0,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the tracing subscriber.
    tracing_subscriber::fmt::init();

    let pool = if let Some(url) = get_optional_env("DATABASE_URL") {
        debug!("DATABASE_URL set, connecting to database");

        let pool = MySqlPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(5))
            .test_before_acquire(false)
            .connect(&url)
            .await?;

        // Note sure if this makes sense versus just setting the min_size to 1.
        let mut connection = pool
            .acquire()
            .await
            .context("database connection could not be established")?;

        connection.ping().await?;
        drop(connection);

        info!("database connection established");

        Some(pool)
    } else {
        debug!("DATABASE_URL not set");

        None
    };

    let token = get_optional_env("DISCORD_TOKEN").context("missing discord bot token")?;

    // HTTP is separate from the gateway, so create a new client.
    let http = Arc::new(HttpClient::new(token.clone()));

    // Just block on these, it simplifies the startup logic.
    let user = Arc::new(http.current_user().await?.model().await?);
    let (application_id, owners) = get_application_id_and_owners(&http).await?;

    let cache = Arc::new(Cache::new(http.clone()));

    let data_dir = get_optional_env("DATA_DIR").map(PathBuf::from);
    let social = Arc::new(Mutex::new(SocialGraph::new(data_dir)));

    let font_name = get_optional_env("FONT_NAME").unwrap_or("sans-serif".into());

    let management_guild = get_optional_env("MANAGEMENT_GUILD").map(|value| {
        use std::str::FromStr;

        Id::<GuildMarker>::from_str(&value).expect("Invalid MANAGEMENT_GUILD value")
    });

    let http_clone = http.clone();
    tokio::spawn(async move {
        http_clone
            .interaction(application_id)
            .set_global_commands(&[
                Command {
                    application_id: None,
                    default_member_permissions: None,
                    dm_permission: Some(true),
                    description: "Show help info and commands.".to_string(),
                    description_localizations: None,
                    guild_id: None,
                    id: None,
                    kind: CommandType::ChatInput,
                    name: "help".to_string(),
                    name_localizations: None,
                    nsfw: None,
                    options: Vec::new(),
                    version: Id::new(1),
                },
                Command {
                    application_id: None,
                    default_member_permissions: None,
                    dm_permission: Some(false),
                    description: "Get a preview-quality graph image.".to_string(),
                    description_localizations: None,
                    guild_id: None,
                    id: None,
                    kind: CommandType::ChatInput,
                    name: "graph".to_string(),
                    name_localizations: None,
                    nsfw: None,
                    options: vec![CommandOption {
                        autocomplete: None,
                        channel_types: None,
                        choices: Some(vec![
                            CommandOptionChoice {
                                name: "Light".to_string(),
                                name_localizations: None,
                                value: CommandOptionChoiceValue::String("light".into()),
                            },
                            CommandOptionChoice {
                                name: "Dark".to_string(),
                                name_localizations: None,
                                value: CommandOptionChoiceValue::String("dark".into()),
                            },
                            CommandOptionChoice {
                                name: "Transparent Light".to_string(),
                                name_localizations: None,
                                value: CommandOptionChoiceValue::String("transparent light".into()),
                            },
                            CommandOptionChoice {
                                name: "Transparent Dark".to_string(),
                                name_localizations: None,
                                value: CommandOptionChoiceValue::String("transparent dark".into()),
                            },
                        ]),
                        description: "Style of graph to render.".to_string(),
                        description_localizations: None,
                        kind: CommandOptionType::String,
                        max_length: None,
                        max_value: None,
                        min_length: None,
                        min_value: None,
                        name: "style".to_string(),
                        name_localizations: None,
                        options: None,
                        required: Some(false),
                    }],
                    version: Id::new(1),
                },
            ])
            .await
            .expect("failed to setup global commands");

        debug!("setup global commands");
    });

    let intents = Intents::GUILDS
        | Intents::GUILD_MESSAGES
        | Intents::GUILD_MESSAGE_REACTIONS
        | Intents::MESSAGE_CONTENT;

    let config = Config::new(token, intents);

    let mut shards: Vec<_> = stream::create_recommended(&http, config, |_, config| config.build())
        .await?
        .collect();

    // let mut shards: Vec<_> =
    //     stream::create_range(0..3, 3, config, |_, config| config.build()).collect();

    let shard_senders: Vec<_> = shards.iter().map(|shard| shard.sender()).collect();

    let shutdown = Arc::new(AtomicBool::new(false));

    let shutdown_clone = shutdown.clone();
    let shutdown_senders = shard_senders.clone();

    ctrlc::set_handler(move || {
        info!("ctrlc handler called");

        shutdown_clone.store(true, Ordering::Relaxed);

        for sender in &shutdown_senders {
            if let Err(error) = sender.close(CloseFrame::NORMAL) {
                warn!(?error, "failed to send close frame");
            }
        }
    })?;

    let last_presence_update = Arc::new(Mutex::new(LastPresenceUpdate::new()));

    let mut stream = ShardEventStream::new(shards.iter_mut());

    while let Some((shard, event)) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(source) => {
                warn!(?source, "error receiving event");

                // An error may be fatal when something like invalid privileged
                // intents are specified or the Discord token is invalid.
                if source.is_fatal() {
                    break;
                }

                continue;
            }
        };

        // Drop these early just to clean up some logging for development.
        if let Event::GatewayHeartbeatAck = event {
            continue;
        }

        debug!(?event, shard = ?shard.id(), "received event");

        if let Event::GatewayClose(_) = event {
            if shutdown.load(Ordering::Relaxed) {
                // Forget the shard to avoid returning it back to the stream.
                std::mem::forget(shard);
                continue;
            }
        }

        // Update the cache with the event.
        // Done before we spawn the tasks to ensure the cache is updated.
        cache.update(&event);

        let needs_presence_update = matches!(&event, Event::Ready(_) | Event::Resumed);
        let wants_presence_update = matches!(&event, Event::GuildCreate(_) | Event::GuildDelete(_));

        if needs_presence_update || wants_presence_update {
            {
                let mut last_presence_update = last_presence_update.lock();
                let guild_counts = &mut last_presence_update.guild_counts;

                match &event {
                    Event::Ready(_) => *guild_counts.entry(shard.id()).or_insert(0) = 0,
                    Event::GuildCreate(_) => *guild_counts.entry(shard.id()).or_insert(0) += 1,
                    Event::GuildDelete(_) => *guild_counts.entry(shard.id()).or_insert(0) -= 1,
                    _ => {}
                }
            }

            let shard_senders = shard_senders.clone();
            let last_presence_update = last_presence_update.clone();

            tokio::spawn(async move {
                if !needs_presence_update {
                    let last = last_presence_update.lock().when;
                    tokio::time::sleep_until(last + Duration::from_secs(2)).await;
                }

                let mut last_presence_update = last_presence_update.lock();

                let count = last_presence_update.guild_counts.values().sum();
                if !needs_presence_update && count == last_presence_update.count {
                    return;
                }

                let activity: Activity = MinimalActivity {
                    kind: ActivityType::Watching,
                    name: format!(
                        "{} {}",
                        format_guild_count(count),
                        if count == 1 { "server" } else { "servers" }
                    ),
                    url: None,
                }
                .into();

                debug!("sending presence update with {} guilds", count);

                let message = UpdatePresence::new(vec![activity], false, 0, Status::Online)
                    .expect("malformed presence payload");

                for shard in &shard_senders {
                    if let Err(error) = shard.command(&message) {
                        warn!(?error, ?message, "failed to update presence");
                    }
                }

                last_presence_update.when = Instant::now();
                last_presence_update.count = count;
            });
        }

        let context = Context {
            shard: shard.sender(),
            application_id,
            user: user.clone(),
            owners: owners.clone(),
            management_guild,
            http: http.clone(),
            cache: cache.clone(),
            social: social.clone(),
            pool: pool.clone(),
            font_name: font_name.clone(),
        };

        tokio::spawn(async move {
            if let Err(error) = handle_event(&context, &event).await {
                error!("error handling event {:?}: {:?}", event.kind(), error);
            }
        });
    }

    info!("event stream ended, exiting");

    Ok(())
}

async fn get_application_id_and_owners(
    http: &Client,
) -> Result<(Id<ApplicationMarker>, HashSet<Id<UserMarker>>)> {
    let info = http.current_user_application().await?.model().await?;

    let mut owners = HashSet::new();

    if let Some(team) = &info.team {
        for member in &team.members {
            if member.membership_state == TeamMembershipState::Accepted {
                owners.insert(member.user.id);
            }
        }
    } else if let Some(owner) = &info.owner {
        owners.insert(owner.id);
    }

    Ok((info.id, owners))
}

async fn handle_event(context: &Context, event: &Event) -> Result<()> {
    if commands::handle_event(context, event).await? {
        // If the command processor consumed it, don't do any more processing.
        return Ok(());
    }

    social::handle_event(context, event).await?;

    Ok(())
}

fn format_guild_count(count: usize) -> String {
    if count > 100_000 {
        format!("{:.0}k", (count as f64) / 1000.0)
    } else if count > 10_000 {
        format!("{:.1}k", (count as f64) / 1000.0)
    } else if count > 1_000 {
        format!("{},{:0.3}", count / 1000, count - ((count / 1000) * 1000))
    } else {
        format!("{}", count)
    }
}

#[cfg(test)]
mod tests {
    use super::format_guild_count;

    #[test]
    fn test_format_guild_count_small() {
        assert_eq!(format_guild_count(5), "5");
    }

    #[test]
    fn test_format_guild_count_large() {
        assert_eq!(format_guild_count(1450), "1,450");
    }

    #[test]
    fn test_format_guild_count_huge() {
        assert_eq!(format_guild_count(12345), "12.3k");
    }

    #[test]
    fn test_format_guild_count_gigantic() {
        assert_eq!(format_guild_count(123456), "123k");
    }
}

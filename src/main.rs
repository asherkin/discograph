mod cache;
mod commands;
mod context;
mod social;

use anyhow::{Context as AnyhowContext, Result};
use parking_lot::Mutex;
use sqlx::mysql::MySqlPoolOptions;
use sqlx::Connection;
use tracing::{debug, error, info, warn};
use twilight_gateway::{Config, Event, Shard};
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

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

    // Configure gateway connection.
    // TODO: Bring back multiple shards.
    let mut shard = Shard::with_config(ShardId::ONE, config);

    let shutdown = Arc::new(AtomicBool::new(false));

    let shutdown_clone = shutdown.clone();
    ctrlc::set_handler(move || {
        info!("ctrlc handler called");

        shutdown_clone.store(true, Ordering::Relaxed);
    })?;

    let mut close_sent = false;

    loop {
        let event = shard.next_event().await;

        if shutdown.load(Ordering::Relaxed) {
            if !close_sent {
                let _ = shard.close(CloseFrame::NORMAL).await;
                close_sent = true;
            }

            if let Ok(Event::GatewayClose(_)) = event {
                // Calling next_event() after GatewayClose will reconnect.
                break;
            }
        }

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

        // Update the cache with the event.
        // Done before we spawn the tasks to ensure the cache is updated.
        cache.update(&event);

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
    if let Event::Ready(_) | Event::Resumed = event {
        let activity: Activity = MinimalActivity {
            kind: ActivityType::Watching,
            name: format!("| @{} help", context.user.name),
            url: None,
        }
        .into();

        let message = UpdatePresence::new(vec![activity], false, 0, Status::Online)?;

        context.shard.command(&message)?;
    }

    if commands::handle_event(context, event).await? {
        // If the command processor consumed it, don't do any more processing.
        return Ok(());
    }

    social::handle_event(context, event).await?;

    Ok(())
}

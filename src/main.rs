mod cache;
mod commands;
mod context;
mod social;
mod stats;

use anyhow::{Context as AnyhowContext, Result};
use futures::StreamExt;
use parking_lot::Mutex;
use sqlx::mysql::MySqlPoolOptions;
use sqlx::Connection;
use tracing::{debug, error, info, warn};
use twilight_gateway::stream::ShardEventStream;
use twilight_gateway::{stream, Config, Event};
use twilight_http::{Client as HttpClient, Client};
use twilight_model::application::command::{
    Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue, CommandOptionType,
    CommandType,
};
use twilight_model::gateway::payload::outgoing::update_presence::UpdatePresencePayload;
use twilight_model::gateway::presence::{Activity, ActivityType, MinimalActivity, Status};
use twilight_model::gateway::{CloseFrame, Intents};
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

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the tracing subscriber.
    tracing_subscriber::fmt::init();

    let pool = if let Some(url) = get_optional_env("DATABASE_URL") {
        debug!("DATABASE_URL set, connecting to database");

        let pool = MySqlPoolOptions::new()
            .acquire_timeout(Duration::from_secs(5))
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

        // Only do this if we're not resuming a gateway connection.
        if let Err(error) = stats::reset_guilds(&pool).await {
            error!(?error, "failed to reset guild stats");
        }

        Some(pool)
    } else {
        debug!("DATABASE_URL not set");

        None
    };

    let token = get_optional_env("DISCORD_TOKEN").context("missing discord bot token")?;

    let mut http = HttpClient::builder().token(token.clone());

    if let Some(proxy) = get_optional_env("DISCORD_PROXY") {
        http = http.proxy(proxy, true);
    }

    // HTTP is separate from the gateway, so create a new client.
    let http = Arc::new(http.build());

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

    let guilds_with_broken_commands = Arc::new(Mutex::new(HashMap::new()));
    let channels_with_debug_enabled = Arc::new(Mutex::new(HashSet::new()));

    tokio::spawn(setup_global_commands(http.clone(), application_id));

    let mut intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::GUILD_MESSAGE_REACTIONS;

    if let Some("1") = get_optional_env("DISCOGRAPH_SERVER_MEMBERS").as_deref() {
        intents |= Intents::GUILD_MEMBERS;
    }

    if let Some("1") = get_optional_env("DISCOGRAPH_MESSAGE_CONTENT").as_deref() {
        intents |= Intents::MESSAGE_CONTENT;
    }

    let presence = UpdatePresencePayload::new(
        vec![Activity::from(MinimalActivity {
            kind: ActivityType::Watching,
            name: "for /graph".into(),
            url: None,
        })],
        false,
        0,
        Status::Online,
    )
    .expect("malformed presence payload");

    let config = Config::builder(token, intents).presence(presence).build();

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

    if let (Some(token), Some(bot_id)) = (
        get_optional_env("DISCOGRAPH_TOPGG_TOKEN"),
        get_optional_env("DISCOGRAPH_TOPGG_BOT_ID"),
    ) {
        tokio::spawn(start_posting_stats(token, bot_id, cache.clone()));
    } else {
        debug!("top.gg stats posting not configured");
    }

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
            guilds_with_broken_commands: guilds_with_broken_commands.clone(),
            channels_with_debug_enabled: channels_with_debug_enabled.clone(),
        };

        // We have to do this outside of the future as otherwise the events might not be ordered.
        if let Err(error) = stats::handle_event(&context, &event).await {
            error!(?error, "failed to update guild stats");
        }

        tokio::spawn(async move {
            if let Err(error) = handle_event(&context, &event).await {
                error!("error handling event {:?}: {:?}", event.kind(), error);
            }
        });
    }

    info!("event stream ended, exiting");

    if let Some(pool) = &pool {
        if let Err(error) = stats::set_offline(pool).await {
            warn!(?error, "failed to set guilds offline");
        }
    }

    Ok(())
}

async fn start_posting_stats(token: String, bot_id: String, cache: Arc<Cache>) {
    use dbl::types::ShardStats;

    let client = dbl::Client::new(token).expect("failed to create top.gg api client");

    let bot_id: u64 = bot_id.parse().expect("invalid top.gg bot id format");

    info!("starting posting stats to top.gg for {}", bot_id);

    // Wait 5 minutes for most guilds to have connected.
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;

    loop {
        let server_count = cache.get_guild_count();

        info!(?server_count, "posting stats to top.gg");

        let result = client
            .update_stats(
                bot_id,
                ShardStats::Cumulative {
                    server_count: server_count as u64,
                    shard_count: None,
                },
            )
            .await;

        if let Err(error) = result {
            warn!(?error, "failed to post stats to top.gg");
        }

        // 30 minutes between updates.
        tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    }
}

async fn setup_global_commands(http: Arc<Client>, application_id: Id<ApplicationMarker>) {
    http.interaction(application_id)
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

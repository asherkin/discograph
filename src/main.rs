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
use twilight_model::gateway::payload::outgoing::UpdatePresence;
use twilight_model::gateway::presence::{Activity, ActivityType, MinimalActivity, Status};
use twilight_model::gateway::{CloseFrame, Intents, ShardId};
use twilight_model::id::marker::UserMarker;
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
    let owners = Arc::new(get_application_owners(&http).await?);

    let cache = Arc::new(Cache::new(http.clone()));

    let data_dir = get_optional_env("DATA_DIR").map(PathBuf::from);
    let social = Arc::new(Mutex::new(SocialGraph::new(data_dir)));

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

        if let Event::Ready(event) = &event {
            let activity: Activity = MinimalActivity {
                kind: ActivityType::Watching,
                name: format!("| @{} help", event.user.name),
                url: None,
            }
            .into();

            let message = UpdatePresence::new(vec![activity], false, 0, Status::Online)?;

            shard.command(&message).await?;
        }

        // Update the cache with the event.
        // Done before we spawn the tasks to ensure the cache is updated.
        cache.update(&event);

        let context = Context {
            user: user.clone(),
            owners: owners.clone(),
            http: http.clone(),
            cache: cache.clone(),
            social: social.clone(),
            pool: pool.clone(),
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

async fn get_application_owners(http: &Client) -> Result<HashSet<Id<UserMarker>>> {
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

    Ok(owners)
}

async fn handle_event(context: &Context, event: &Event) -> Result<()> {
    if commands::handle_event(context, event).await? {
        // If the command processor consumed it, don't do any more processing.
        return Ok(());
    }

    social::handle_event(context, event).await?;

    Ok(())
}

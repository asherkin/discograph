mod cache;
mod commands;
mod context;
mod social;

use anyhow::{Context as AnyhowContext, Result};
use parking_lot::Mutex;
use tokio::stream::StreamExt;
use tracing::{error, info};
use twilight_gateway::{cluster::Cluster, Event};
use twilight_http::{Client as HttpClient, Client};
use twilight_model::gateway::presence::ActivityType;
use twilight_model::gateway::Intents;
use twilight_model::id::UserId;
use twilight_model::oauth::team::TeamMembershipState;

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use crate::cache::Cache;
use crate::context::Context;
use crate::social::graph::SocialGraph;

fn get_optional_env(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => panic!(error.to_string()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the tracing subscriber.
    tracing_subscriber::fmt::init();

    let token = get_optional_env("DISCORD_TOKEN").context("missing discord bot token")?;

    // HTTP is separate from the gateway, so create a new client.
    let http = HttpClient::new(&token);

    // Just block on these, it simplifies the startup logic.
    let user = Arc::new(http.current_user().await?);
    let owners = Arc::new(get_application_owners(&http).await?);

    let cache = Arc::new(Cache::new(http.clone()));

    let data_dir = get_optional_env("DATA_DIR").map(PathBuf::from);
    let social = Arc::new(Mutex::new(SocialGraph::new(data_dir)));

    // Configure gateway connection.
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::GUILD_MESSAGE_REACTIONS;
    let cluster = Cluster::builder(&token, intents).build().await?;

    // Start all shards in the cluster in the background.
    // This *must* be after any other awaits, otherwise we can lose events.
    let cluster_spawn = cluster.clone();
    tokio::spawn(async move {
        cluster_spawn.up().await;
    });

    let cluster_down = cluster.clone();
    ctrlc::set_handler(move || {
        info!("ctrlc handler called");

        cluster_down.down();
    })?;

    let mut events = cluster.events();

    // Process each event as they come in.
    while let Some((shard_id, event)) = events.next().await {
        // Drop these early just to clean up some logging for development.
        if let Event::GatewayHeartbeatAck = event {
            continue;
        }

        info!("received event: {:?}", event);

        // Update the cache with the event.
        // Done before we spawn the tasks to ensure the cache is updated.
        cache.update(&event);

        let context = Context {
            user: user.clone(),
            owners: owners.clone(),
            shard: cluster.shard(shard_id).unwrap(),
            http: http.clone(),
            cache: cache.clone(),
            social: social.clone(),
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

async fn get_application_owners(http: &Client) -> Result<HashSet<UserId>> {
    let info = http.current_user_application().await?;

    let mut owners = HashSet::new();

    if let Some(team) = &info.team {
        for member in &team.members {
            if member.membership_state == TeamMembershipState::Accepted {
                owners.insert(member.user.id);
            }
        }
    } else {
        owners.insert(info.owner.id);
    }

    Ok(owners)
}

async fn handle_event(context: &Context, event: &Event) -> Result<()> {
    if let Event::Ready(event) = event {
        context
            .set_activity(
                ActivityType::Watching,
                format!("| @{} help", event.user.name),
            )
            .await?;
    }

    if commands::handle_event(context, event).await? {
        // If the command processor consumed it, don't do any more processing.
        return Ok(());
    }

    social::handle_event(context, event).await?;

    Ok(())
}

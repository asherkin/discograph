mod bot;
mod cache;
mod inference;
mod parsing;
mod social;

use anyhow::{Context, Result};
use env_logger::Env;
use log::info;
use serenity::client::Client;

use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use crate::bot::BotEnvironment;

fn get_optional_env(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => panic!(error.to_string()),
    }
}

fn main() -> Result<()> {
    // RUST_LOG = reqwest=debug,tungstenite::protocol=trace/^(Received|response)
    env_logger::from_env(Env::default().default_filter_or("info")).try_init()?;

    // Permissions to request: 117824
    // * View Channels [required]
    // * Send Messages
    // * Embed Links
    // * Attach Files
    // * Read Message History [required]
    // * Add Reactions

    let token = get_optional_env("DISCORD_TOKEN").context("missing discord bot token")?;

    let data_dir = get_optional_env("DATA_DIR").map(PathBuf::from);

    let environment = get_optional_env("BOT_ENV").map_or(BotEnvironment::Development, |value| {
        match value.as_ref() {
            "production" => BotEnvironment::Production,
            "development" => BotEnvironment::Development,
            value => panic!(format!("unknown environment: {}", value)),
        }
    });

    let notification = Arc::new((Mutex::new(false), Condvar::new()));

    let mut client = Client::new_with_extras(&token, |extras| {
        // TODO: Replace `guild_subscriptions` with `intents` once it is released in a serenity version.
        extras
            .event_handler(bot::Handler::new(
                data_dir,
                environment,
                notification.clone(),
            ))
            .guild_subscriptions(false)
    })?;

    let shard_manager = client.shard_manager.clone();
    ctrlc::set_handler(move || {
        let mut shard_manager = shard_manager.lock();

        shard_manager.shutdown_all();
    })?;

    let gateway_info = client.cache_and_http.http.get_bot_gateway()?;

    info!("{:?}", gateway_info.session_start_limit);

    // We don't use `start_autosharded` so we can log the rate limit info.
    info!("starting {} shards", gateway_info.shards);
    client.start_shards(gateway_info.shards)?;

    info!("shards have apparently finished");

    // The Handler instance is dropped elsewhere when the shards are shutdown,
    // but waiting on the threadpool doesn't block on all the Drop implementations
    // running. This condvar (which is signalled by a Drop impl on a field in
    // Handler seems to be the only reliable way to wait for all the other Handler
    // fields to be dropped - which is important once we introduce SQLite.
    let (lock, cvar) = &*notification;
    let mut finished = lock.lock().unwrap();
    while !*finished {
        finished = cvar.wait(finished).unwrap();
    }

    info!("cleanup finished, exiting main");
    Ok(())
}

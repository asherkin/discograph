mod bot;
mod cache;
mod inference;
mod parsing;
mod social;

use serenity::client::Client;

use std::env;
use std::path::PathBuf;

use crate::bot::BotEnvironment;

fn get_optional_env(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => panic!(error.to_string()),
    }
}

fn main() {
    // RUST_LOG = reqwest=debug,tungstenite::protocol=trace/^(Received|response)
    env_logger::init();

    // Permissions to request: 85056
    // * View Channels [required]
    // * Send Messages
    // * Embed Links
    // * Read Message History [required]
    // * Add Reactions

    let token = get_optional_env("DISCORD_TOKEN").expect("missing discord bot token");

    let data_dir = get_optional_env("DATA_DIR").map(|value| PathBuf::from(value));

    let environment = get_optional_env("BOT_ENV").map_or(BotEnvironment::Development, |value| {
        match value.as_ref() {
            "production" => BotEnvironment::Production,
            "development" => BotEnvironment::Development,
            value => panic!(format!("unknown environment: {}", value)),
        }
    });

    let mut client = Client::new_with_extras(&token, |extras| {
        // TODO: Replace `guild_subscriptions` with `intents` once it is released in a serenity version.
        extras
            .event_handler(bot::Handler::new(data_dir, environment))
            .guild_subscriptions(false)
    })
    .unwrap();

    let gateway_info = client.cache_and_http.http.get_bot_gateway().unwrap();

    println!("{:?}", gateway_info.session_start_limit);

    // We don't use `start_autosharded` so we can log the rate limit info.
    println!("starting {} shards", gateway_info.shards);
    client.start_shards(gateway_info.shards).unwrap();
}

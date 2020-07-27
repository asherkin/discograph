use serenity::client::Client;

use std::env;

mod bot;
mod cache;
mod inference;
mod parsing;

fn main() {
    // RUST_LOG = reqwest=debug,tungstenite::protocol=trace/^(Received|response)
    env_logger::init();

    // Permissions to request: 85056
    // * View Channels [required]
    // * Send Messages
    // * Embed Links
    // * Read Message History [required]
    // * Add Reactions

    let mut client = Client::new_with_extras(&env::var("DISCORD_TOKEN").expect("token"), |f| {
        f.event_handler(bot::Handler::new())
            .guild_subscriptions(false) // TODO: Replace this with `intents` once it is released in a serenity version.
    })
    .unwrap();

    let gateway_info = client.cache_and_http.http.get_bot_gateway().unwrap();

    println!("{:?}", gateway_info.session_start_limit);

    // We don't use `start_autosharded` so we can log the rate limit info.
    println!("starting {} shards", gateway_info.shards);
    client.start_shards(gateway_info.shards).unwrap();
}

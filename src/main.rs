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

    client.start_autosharded().unwrap();
}

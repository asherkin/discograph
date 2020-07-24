use serenity::client::Client;

use std::env;

mod bot;
mod cache;
mod inference;

fn main() {
    // RUST_LOG = reqwest=debug,tungstenite::protocol=trace/^(Received|response)
    env_logger::init();

    // Permissions to request: 85056
    // * View Channels
    // * Send Messages
    // * Embed Links
    // * Read Message History
    // * Add Reactions

    // Login with a bot token from the environment
    let mut client = Client::new_with_extras(&env::var("DISCORD_TOKEN").expect("token"), |f| {
        f.event_handler(bot::Handler::new())
            .guild_subscriptions(false) // TODO: Replace this with `intents` once it is released in a serenity version.
    })
    .expect("Error creating client");

    // start listening for events by starting a single shard
    if let Err(why) = client.start() {
        println!("An error occurred while running the client: {:?}", why);
    }
}

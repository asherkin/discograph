use parking_lot::Mutex;
use sqlx::MySqlPool;
use tokio::time::Instant;
use twilight_gateway::MessageSender;
use twilight_http::Client;
use twilight_model::id::marker::{ApplicationMarker, GuildMarker, UserMarker};
use twilight_model::id::Id;
use twilight_model::user::CurrentUser;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::cache::Cache;
use crate::social::graph::SocialGraph;

#[derive(Clone)]
pub struct Context {
    pub shard: MessageSender,
    pub application_id: Id<ApplicationMarker>,
    pub user: Arc<CurrentUser>,
    pub owners: HashSet<Id<UserMarker>>,
    pub management_guild: Option<Id<GuildMarker>>,
    pub http: Arc<Client>,
    pub cache: Arc<Cache>,
    pub social: Arc<Mutex<SocialGraph>>,
    pub pool: Option<MySqlPool>,
    pub font_name: String,
    pub guilds_with_broken_commands: Arc<Mutex<HashMap<Id<GuildMarker>, Option<Instant>>>>,
}

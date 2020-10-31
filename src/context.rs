use parking_lot::Mutex;
use sqlx::MySqlPool;
use twilight_gateway::shard::CommandError;
use twilight_gateway::Shard;
use twilight_http::Client;
use twilight_model::gateway::payload::UpdateStatus;
use twilight_model::gateway::presence::{Activity, ActivityType, Status};
use twilight_model::id::UserId;
use twilight_model::user::CurrentUser;

use std::collections::HashSet;
use std::sync::Arc;

use crate::cache::Cache;
use crate::social::graph::SocialGraph;

#[derive(Clone)]
pub struct Context {
    pub user: Arc<CurrentUser>,
    pub owners: Arc<HashSet<UserId>>,
    pub shard: Shard,
    pub http: Client,
    pub cache: Arc<Cache>,
    pub social: Arc<Mutex<SocialGraph>>,
    pub pool: Option<MySqlPool>,
}

impl Context {
    pub async fn set_activity(&self, kind: ActivityType, name: String) -> Result<(), CommandError> {
        let activity = Activity {
            application_id: None,
            assets: None,
            created_at: None,
            details: None,
            emoji: None,
            flags: None,
            id: None,
            instance: None,
            kind,
            name,
            party: None,
            secrets: None,
            state: None,
            timestamps: None,
            url: None,
        };

        let message = UpdateStatus::new(vec![activity], false, 0, Status::Online);

        self.shard.command(&message).await
    }
}

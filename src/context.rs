use parking_lot::Mutex;
use sqlx::MySqlPool;
use twilight_http::Client;
use twilight_model::id::marker::UserMarker;
use twilight_model::id::Id;
use twilight_model::user::CurrentUser;

use std::collections::HashSet;
use std::sync::Arc;

use crate::cache::Cache;
use crate::social::graph::SocialGraph;

#[derive(Clone)]
pub struct Context {
    pub user: Arc<CurrentUser>,
    pub owners: Arc<HashSet<Id<UserMarker>>>,
    pub http: Arc<Client>,
    pub cache: Arc<Cache>,
    pub social: Arc<Mutex<SocialGraph>>,
    pub pool: Option<MySqlPool>,
}

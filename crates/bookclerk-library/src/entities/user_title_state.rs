//! SeaORM entity for the `user_title_state` table.

use sea_orm::entity::prelude::*;

/// One user's own state for one work: finished flag, ratings and tags.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "user_title_state")]
pub struct Model {
    /// The user (`users.id`) the state belongs to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The work (`works.id`) the state is about.
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// `1` once the user finished the work, else `0`.
    pub is_finished: i64,
    /// The user's overall rating (storefront scale, 0 to 5), when given.
    pub rating_overall: Option<f64>,
    /// The user's narration rating (0 to 5), when given.
    pub rating_performance: Option<f64>,
    /// The user's story rating (0 to 5), when given.
    pub rating_story: Option<f64>,
    /// The user's tags, as one serialized string, when any.
    pub tags: Option<String>,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

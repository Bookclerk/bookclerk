//! SeaORM entity for `cluster_links`: on a host node, the owner clusters it
//! enrolled into (written by `control_plane::record_cluster_link`).

use sea_orm::entity::prelude::*;

/// Row shape for one link from this install to an owner cluster.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "cluster_links")]
pub struct Model {
    /// Owner cluster id returned by enrollment.
    #[sea_orm(primary_key, auto_increment = false)]
    pub owner_cluster_id: String,
    /// Owner origin (`https://owner.example`).
    pub owner_origin: String,
    /// Node id the owner minted for this install.
    pub remote_node_id: String,
    /// SPKI pin from the enrollment link, when it carried one.
    pub spki_pin: Option<String>,
    /// `active`, `revoked`, or `left`.
    pub state: String,
    /// RFC 3339 enrollment time.
    pub enrolled_at: String,
    /// "Share this branch's books with *library*" (1 = on, the default).
    pub share_books: i64,
    /// "Include books from my own branches" (1 = on, the default).
    pub include_branches: i64,
    /// The owner library's ancestry (JSON list of cluster ids, the owner's
    /// own first), as it last reported it.
    pub ancestry_json: String,
    /// The most jobs this install runs for the library at once, set by its
    /// operator; `NULL` leaves it to the equal share of the machine.
    pub max_jobs: Option<i64>,
    /// "List my members' books to *library*" (1 = on, the default): the
    /// branch answers that library's catalog lookups with its members'
    /// books as well as its owner's.
    pub list_members_books: i64,
    /// "Use this computer to download" (1 = on, the default): the link
    /// takes that library's jobs and runs its plugins here. Off makes this
    /// install a window of that library: no jobs, no plugins, and it reads
    /// the books the library holds for its person instead.
    pub download: i64,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

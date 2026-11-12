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
    /// "Download for other members of *library*" (0 = off, the default):
    /// the branch takes work for the library's other members too, not only
    /// its owner's.
    pub work_for_members: i64,
    /// "Keep copies of other members' books" (0 = off, the default): a
    /// book acquired here for another member is also written to this
    /// install's own destinations.
    pub keep_member_copies: i64,
    /// The most jobs this install runs for the library at once, set by its
    /// operator; `NULL` leaves it to the equal share of the machine.
    pub max_jobs: Option<i64>,
    /// "List my members' books to *library*" (1 = on, the default): the
    /// branch answers that library's catalog lookups with its members'
    /// books as well as its owner's.
    pub list_members_books: i64,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

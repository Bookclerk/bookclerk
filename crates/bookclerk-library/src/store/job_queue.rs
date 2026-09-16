//! Durable job-queue methods on [`LibraryStore`].

use chrono::{Duration, Utc};
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{NotSet, Set},
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use uuid::Uuid;

use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::entities::{books, job_temp_paths, jobs};
use crate::error::{LibraryError, Result};
use crate::models::{
    job_backoff_run_after, AcquireStatus, BookRecord, EnqueueJobSpec, EnqueueOutcome, JobFence,
    JobKind, JobPayload, JobRecord, JobResourceClass, JobState, JobTempPath, JobTrigger,
    JOB_PAYLOAD_VERSION,
};

impl LibraryStore {
    /// Admit a job in one transaction, coalescing onto an active dedup key.
    ///
    /// # Arguments
    ///
    /// * `spec` - Kind, payload, priority, attempt cap, and pending cap.
    ///
    /// # Returns
    ///
    /// [`EnqueueOutcome::Created`], [`EnqueueOutcome::Duplicate`], or
    /// [`EnqueueOutcome::QueueFull`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the database write fails.
    pub async fn enqueue_job(&self, spec: EnqueueJobSpec) -> Result<EnqueueOutcome> {
        if let Some(atomic) = &self.atomic {
            return atomic.enqueue_job(spec).await;
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match enqueue_job_on(&txn, spec).await {
            Ok(outcome) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(outcome)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Claim the next ready job with a conditional `pending` → `running` mutation.
    ///
    /// `operation_id` is the atomic / local-txn idempotency key: retrying a
    /// lost response with the same id must not claim a different row.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the claim write fails.
    pub async fn claim_next_job(
        &self,
        resource_class: JobResourceClass,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
    ) -> Result<Option<JobRecord>> {
        if let Some(atomic) = &self.atomic {
            return atomic
                .claim_next_job(resource_class, owner, lease_secs, operation_id)
                .await;
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match claim_next_job_on(&txn, resource_class, owner, lease_secs).await {
            Ok(job) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(job)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Refresh the lease when `fence` still owns the running generation.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn heartbeat_job(
        &self,
        fence: &JobFence,
        lease_secs: u64,
        progress: Option<&str>,
    ) -> Result<bool> {
        let now = Utc::now();
        let mut update = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Some(
                    (now + Duration::seconds(i64::try_from(lease_secs).unwrap_or(60))).to_rfc3339(),
                )),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now.to_rfc3339()),
            )
            .filter(jobs::Column::Id.eq(&fence.job_id))
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .filter(jobs::Column::LeaseOwner.eq(&fence.owner))
            .filter(jobs::Column::LeaseGeneration.eq(fence.generation));
        if let Some(progress) = progress {
            update = update.col_expr(
                jobs::Column::Progress,
                sea_orm::sea_query::Expr::value(Some(progress.to_string())),
            );
        }
        let res = update.exec(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Persist progress when `fence` still owns the running generation.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn set_job_progress(&self, fence: &JobFence, progress: &str) -> Result<bool> {
        let res = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::Progress,
                sea_orm::sea_query::Expr::value(Some(progress.to_string())),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now_str()),
            )
            .filter(jobs::Column::Id.eq(&fence.job_id))
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .filter(jobs::Column::LeaseOwner.eq(&fence.owner))
            .filter(jobs::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Mark a running job succeeded when `fence` still owns the generation.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn complete_job(&self, fence: &JobFence, progress: Option<&str>) -> Result<bool> {
        let now = now_str();
        let mut update = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::State,
                sea_orm::sea_query::Expr::value(JobState::Succeeded.as_str()),
            )
            .col_expr(
                jobs::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::FinishedAt,
                sea_orm::sea_query::Expr::value(Some(now.clone())),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(jobs::Column::Id.eq(&fence.job_id))
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .filter(jobs::Column::LeaseOwner.eq(&fence.owner))
            .filter(jobs::Column::LeaseGeneration.eq(fence.generation));
        if let Some(progress) = progress {
            update = update.col_expr(
                jobs::Column::Progress,
                sea_orm::sea_query::Expr::value(Some(progress.to_string())),
            );
        }
        let res = update.exec(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Park a running job as `pending` with a checkpoint, to be claimed again after `wake_at`.
    ///
    /// The CAS matches [`Self::complete_job`]: `state=running` and the caller's fence. On success the
    /// lease is cleared so another replica can claim the next attempt. `payload.checkpoint` is
    /// replaced with `checkpoint` (other payload fields are preserved).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read or update fails, or [`LibraryError::Other`]
    /// when the stored payload is not valid JSON.
    pub async fn suspend_job(
        &self,
        fence: &JobFence,
        checkpoint: &bookclerk_plugin_abi::JobCheckpoint,
        wake_at: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let Some(model) = jobs::Entity::find_by_id(&fence.job_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        if model.state != JobState::Running.as_str()
            || model.lease_owner.as_deref() != Some(fence.owner.as_str())
            || model.lease_generation != fence.generation
        {
            return Ok(false);
        }
        let mut payload: JobPayload = serde_json::from_str(&model.payload)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("job payload: {err}")))?;
        if checkpoint.json.len() > bookclerk_plugin_abi::MAX_CHECKPOINT_BYTES as usize {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "checkpoint of {} bytes exceeds {}",
                checkpoint.json.len(),
                bookclerk_plugin_abi::MAX_CHECKPOINT_BYTES
            )));
        }
        payload.checkpoint = Some(checkpoint.clone());
        payload.resume_pending = true;
        payload.invocation_sequence =
            Some(payload.invocation_sequence.unwrap_or(1).saturating_add(1));
        let payload_json = serde_json::to_string(&payload)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("job payload: {err}")))?;
        let now = now_str();
        let res = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::State,
                sea_orm::sea_query::Expr::value(JobState::Pending.as_str()),
            )
            .col_expr(
                jobs::Column::Payload,
                sea_orm::sea_query::Expr::value(payload_json),
            )
            .col_expr(
                jobs::Column::RunAfter,
                sea_orm::sea_query::Expr::value(wake_at.to_rfc3339()),
            )
            .col_expr(
                jobs::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::Progress,
                sea_orm::sea_query::Expr::value(Some("suspended".to_string())),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(jobs::Column::Id.eq(&fence.job_id))
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .filter(jobs::Column::LeaseOwner.eq(&fence.owner))
            .filter(jobs::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Fail a running job when `fence` still owns the generation.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn fail_job(
        &self,
        fence: &JobFence,
        error_kind: &str,
        error_message: &str,
    ) -> Result<bool> {
        let Some(model) = jobs::Entity::find_by_id(&fence.job_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        if model.state != JobState::Running.as_str()
            || model.lease_owner.as_deref() != Some(fence.owner.as_str())
            || model.lease_generation != fence.generation
        {
            return Ok(false);
        }
        if model.cancel_requested != 0 {
            return self
                .mark_job_cancelled_cas(
                    &fence.job_id,
                    Some(fence),
                    "cancelled",
                    "cancelled by operator",
                    None,
                )
                .await
                .map(|_| true);
        }
        let now = Utc::now();
        let retry = model.attempt_count < model.max_attempts;
        let attempt_count = model.attempt_count;
        let next_state = if retry {
            JobState::Pending.as_str()
        } else {
            JobState::Failed.as_str()
        };
        let run_after = if retry {
            job_backoff_run_after(attempt_count, now).to_rfc3339()
        } else {
            model.run_after.clone()
        };
        let finished = if retry { None } else { Some(now.to_rfc3339()) };
        let retry_payload = if retry {
            let mut payload: JobPayload = serde_json::from_str(&model.payload).unwrap_or_default();
            if payload.resume_pending {
                payload.resume_pending = false;
                Some(
                    serde_json::to_string(&payload).map_err(|err| {
                        LibraryError::Other(anyhow::anyhow!("job payload: {err}"))
                    })?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let mut update = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::State,
                sea_orm::sea_query::Expr::value(next_state),
            )
            .col_expr(
                jobs::Column::ErrorKind,
                sea_orm::sea_query::Expr::value(Some(error_kind.to_string())),
            )
            .col_expr(
                jobs::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Some(error_message.to_string())),
            )
            .col_expr(
                jobs::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::RunAfter,
                sea_orm::sea_query::Expr::value(run_after),
            )
            .col_expr(
                jobs::Column::FinishedAt,
                sea_orm::sea_query::Expr::value(finished),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now.to_rfc3339()),
            );
        if let Some(payload_json) = retry_payload {
            update = update.col_expr(
                jobs::Column::Payload,
                sea_orm::sea_query::Expr::value(payload_json),
            );
        }
        let res = update
            .filter(jobs::Column::Id.eq(&fence.job_id))
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .filter(jobs::Column::LeaseOwner.eq(&fence.owner))
            .filter(jobs::Column::LeaseGeneration.eq(fence.generation))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }

    /// Cancel a pending job immediately, or flag a running job for cooperative stop.
    ///
    /// Pending and running are handled atomically against claim: a CAS miss
    /// (the worker claimed, or the job left `running`) retries until the row
    /// is terminal, cancelled, or flagged `cancel_requested`.
    ///
    /// # Returns
    ///
    /// The updated record, or `None` when the id is unknown.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn request_job_cancel(&self, id: &str) -> Result<Option<JobRecord>> {
        for _ in 0..16 {
            let Some(model) = jobs::Entity::find_by_id(id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            else {
                return Ok(None);
            };
            let state = JobState::parse(&model.state).unwrap_or(JobState::Failed);
            if state.is_terminal() {
                return match try_map_job(model) {
                    Ok(job) => Ok(Some(job)),
                    Err(reason) => {
                        self.mark_invalid_job(id, &reason).await?;
                        self.get_job(id).await
                    }
                };
            }
            if state == JobState::Pending {
                if self
                    .mark_job_cancelled_cas(id, None, "cancelled", "cancelled by operator", None)
                    .await?
                {
                    return self.get_job(id).await;
                }
                continue;
            }
            let res = jobs::Entity::update_many()
                .col_expr(
                    jobs::Column::CancelRequested,
                    sea_orm::sea_query::Expr::value(1i64),
                )
                .col_expr(
                    jobs::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(now_str()),
                )
                .filter(jobs::Column::Id.eq(id))
                .filter(jobs::Column::State.eq(JobState::Running.as_str()))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if res.rows_affected == 1 {
                return self.get_job(id).await;
            }
        }
        self.get_job(id).await
    }

    /// True when a running worker should abort after the current step.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn job_cancel_requested(&self, id: &str) -> Result<bool> {
        let Some(model) = jobs::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        Ok(model.cancel_requested != 0)
    }

    /// Reclaim expired running leases with a conditional `running` update.
    ///
    /// # Returns
    ///
    /// Number of rows moved out of `running`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn reclaim_expired_leases(&self) -> Result<u32> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let running = jobs::Entity::find()
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut n = 0u32;
        for model in running {
            let expired = model
                .lease_expires_at
                .as_deref()
                .map(parse_dt)
                .is_none_or(|exp| exp <= now);
            if !expired {
                continue;
            }
            if model.cancel_requested != 0 {
                if self
                    .mark_job_cancelled_cas(
                        &model.id,
                        Some(&JobFence {
                            job_id: model.id.clone(),
                            owner: model.lease_owner.clone().unwrap_or_default(),
                            generation: model.lease_generation,
                        }),
                        "cancelled",
                        "cancelled by operator",
                        Some(&now_s),
                    )
                    .await?
                {
                    n += 1;
                }
                continue;
            }
            let terminal = model.attempt_count >= model.max_attempts;
            let next_state = if terminal {
                JobState::Failed.as_str()
            } else {
                JobState::Pending.as_str()
            };
            let finished = if terminal { Some(now_s.clone()) } else { None };
            let res = jobs::Entity::update_many()
                .col_expr(
                    jobs::Column::State,
                    sea_orm::sea_query::Expr::value(next_state),
                )
                .col_expr(
                    jobs::Column::LeaseOwner,
                    sea_orm::sea_query::Expr::value(Option::<String>::None),
                )
                .col_expr(
                    jobs::Column::LeaseExpiresAt,
                    sea_orm::sea_query::Expr::value(Option::<String>::None),
                )
                .col_expr(
                    jobs::Column::ErrorKind,
                    sea_orm::sea_query::Expr::value(Some("lease_expired".to_string())),
                )
                .col_expr(
                    jobs::Column::ErrorMessage,
                    sea_orm::sea_query::Expr::value(Some(
                        "worker lease expired; reclaiming after restart".to_string(),
                    )),
                )
                .col_expr(
                    jobs::Column::RunAfter,
                    sea_orm::sea_query::Expr::value(now_s.clone()),
                )
                .col_expr(
                    jobs::Column::FinishedAt,
                    sea_orm::sea_query::Expr::value(finished),
                )
                .col_expr(
                    jobs::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(now_s.clone()),
                )
                .filter(jobs::Column::Id.eq(&model.id))
                .filter(jobs::Column::State.eq(JobState::Running.as_str()))
                .filter(jobs::Column::LeaseGeneration.eq(model.lease_generation))
                .filter(
                    Condition::any()
                        .add(jobs::Column::LeaseExpiresAt.is_null())
                        .add(jobs::Column::LeaseExpiresAt.lte(now_s.clone())),
                )
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if res.rows_affected == 1 {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Load one job by id, rejecting unreadable rows as `invalid_job`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn get_job(&self, id: &str) -> Result<Option<JobRecord>> {
        let Some(model) = jobs::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        match try_map_job(model) {
            Ok(job) => Ok(Some(job)),
            Err(reason) => {
                self.mark_invalid_job(id, &reason).await?;
                let model = jobs::Entity::find_by_id(id)
                    .one(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?;
                Ok(model.and_then(|m| try_map_job(m).ok()))
            }
        }
    }

    /// List recent jobs, newest first, capped at `limit` (minimum 1).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_jobs(&self, limit: u64) -> Result<Vec<JobRecord>> {
        let rows = jobs::Entity::find()
            .order_by_desc(jobs::Column::CreatedAt)
            .limit(limit.max(1))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.id.clone();
            match try_map_job(row) {
                Ok(job) => out.push(job),
                Err(reason) => {
                    self.mark_invalid_job(&id, &reason).await?;
                    if let Some(job) = self.get_job(&id).await? {
                        out.push(job);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Delete terminal jobs older than `retention_days`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn prune_terminal_jobs(&self, retention_days: u64) -> Result<u64> {
        let cutoff =
            (Utc::now() - Duration::days(i64::try_from(retention_days).unwrap_or(7))).to_rfc3339();
        let res = jobs::Entity::delete_many()
            .filter(jobs::Column::State.is_in([
                JobState::Succeeded.as_str(),
                JobState::Failed.as_str(),
                JobState::Cancelled.as_str(),
            ]))
            .filter(jobs::Column::FinishedAt.lte(cutoff))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected)
    }

    /// Reserve `reserved_bytes` against the job temp quota and register `path`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails, or
    /// [`LibraryError::Other`] when the quota would be exceeded.
    pub async fn reserve_job_temp_path(
        &self,
        job_id: &str,
        path: &str,
        reserved_bytes: u64,
        quota_bytes: u64,
    ) -> Result<()> {
        if let Some(atomic) = &self.atomic {
            return atomic
                .reserve_job_temp_path(job_id, path, reserved_bytes, quota_bytes)
                .await;
        }
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match reserve_job_temp_path_on(&txn, job_id, path, reserved_bytes, quota_bytes).await {
            Ok(()) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(())
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Register a scratch path with a zero-byte reservation (legacy helper).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the insert fails.
    pub async fn register_job_temp_path(&self, job_id: &str, path: &str) -> Result<()> {
        self.reserve_job_temp_path(job_id, path, 0, u64::MAX).await
    }

    /// List scratch paths registered for `job_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_job_temp_paths(&self, job_id: &str) -> Result<Vec<JobTempPath>> {
        Ok(job_temp_paths::Entity::find()
            .filter(job_temp_paths::Column::JobId.eq(job_id))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_temp_path)
            .collect())
    }

    /// List every registered scratch path (used by the startup sweeper).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_all_job_temp_paths(&self) -> Result<Vec<JobTempPath>> {
        Ok(job_temp_paths::Entity::find()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_temp_path)
            .collect())
    }

    /// Drop one scratch-path row after that path is gone from disk.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn unregister_job_temp_path(&self, job_id: &str, path: &str) -> Result<()> {
        job_temp_paths::Entity::delete_many()
            .filter(job_temp_paths::Column::JobId.eq(job_id))
            .filter(job_temp_paths::Column::Path.eq(path))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Sum of reserved scratch bytes across all jobs.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn reserved_temp_bytes(&self) -> Result<u64> {
        let rows = job_temp_paths::Entity::find()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|r| u64::try_from(r.reserved_bytes).unwrap_or(0))
            .fold(0u64, u64::saturating_add))
    }

    /// True when any acquire job is currently running.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn has_running_acquire_job(&self) -> Result<bool> {
        let n = jobs::Entity::find()
            .filter(jobs::Column::Kind.eq(JobKind::Acquire.as_str()))
            .filter(jobs::Column::State.eq(JobState::Running.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(n > 0)
    }

    /// Books left in `queued` / `downloading` with no running acquire job.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_orphaned_acquire_books(&self) -> Result<Vec<BookRecord>> {
        if self.has_running_acquire_job().await? {
            return Ok(Vec::new());
        }
        let rows = books::Entity::find()
            .filter(books::Column::AcquireStatus.is_in([
                AcquireStatus::Queued.as_str(),
                AcquireStatus::Downloading.as_str(),
            ]))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(super::map_book(row)?);
        }
        Ok(out)
    }

    /// Mark orphaned `queued` / `downloading` books as `error` after a crash.
    ///
    /// # Returns
    ///
    /// Number of book rows updated.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a status write fails.
    pub async fn reconcile_orphaned_acquire_rows(&self) -> Result<u32> {
        let orphans = self.list_orphaned_acquire_books().await?;
        let mut n = 0u32;
        for book in orphans {
            self.set_acquire_status(
                &book.uuid,
                &book.account_id,
                AcquireStatus::Error,
                book.storage_key.as_deref(),
                Some("orphaned_after_restart"),
            )
            .await?;
            n += 1;
        }
        Ok(n)
    }

    /// Count pending + running jobs (admission cap).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn count_active_jobs(&self) -> Result<i64> {
        count_active_jobs_on(&self.db).await
    }

    /// Rewrite an unreadable command row so it cannot be executed.
    async fn mark_invalid_job(&self, id: &str, reason: &str) -> Result<()> {
        let now = now_str();
        let _ = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::Kind,
                sea_orm::sea_query::Expr::value(JobKind::Invalid.as_str()),
            )
            .col_expr(
                jobs::Column::ResourceClass,
                sea_orm::sea_query::Expr::value(JobResourceClass::Network.as_str()),
            )
            .col_expr(
                jobs::Column::Payload,
                sea_orm::sea_query::Expr::value(invalid_job_payload_json()),
            )
            .col_expr(
                jobs::Column::State,
                sea_orm::sea_query::Expr::value(JobState::Failed.as_str()),
            )
            .col_expr(
                jobs::Column::ErrorKind,
                sea_orm::sea_query::Expr::value(Some("invalid_job".to_string())),
            )
            .col_expr(
                jobs::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Some(reason.to_string())),
            )
            .col_expr(
                jobs::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::FinishedAt,
                sea_orm::sea_query::Expr::value(Some(now.clone())),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(jobs::Column::Id.eq(id))
            .filter(
                jobs::Column::State.is_in([JobState::Pending.as_str(), JobState::Running.as_str()]),
            )
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Cancel when the row is still pending, or still the fenced running attempt.
    ///
    /// When `expired_before` is set, a running cancel also requires
    /// `lease_expires_at` to be null or `<= expired_before` so a live heartbeat
    /// cannot be cancelled by a stale reclaim.
    async fn mark_job_cancelled_cas(
        &self,
        id: &str,
        fence: Option<&JobFence>,
        error_kind: &str,
        error_message: &str,
        expired_before: Option<&str>,
    ) -> Result<bool> {
        let now = now_str();
        let mut update = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::State,
                sea_orm::sea_query::Expr::value(JobState::Cancelled.as_str()),
            )
            .col_expr(
                jobs::Column::CancelRequested,
                sea_orm::sea_query::Expr::value(1i64),
            )
            .col_expr(
                jobs::Column::ErrorKind,
                sea_orm::sea_query::Expr::value(Some(error_kind.to_string())),
            )
            .col_expr(
                jobs::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Some(error_message.to_string())),
            )
            .col_expr(
                jobs::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::FinishedAt,
                sea_orm::sea_query::Expr::value(Some(now.clone())),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(jobs::Column::Id.eq(id));
        update = if let Some(fence) = fence {
            let mut running = update
                .filter(jobs::Column::State.eq(JobState::Running.as_str()))
                .filter(jobs::Column::LeaseOwner.eq(&fence.owner))
                .filter(jobs::Column::LeaseGeneration.eq(fence.generation));
            if let Some(expired_before) = expired_before {
                running = running.filter(
                    Condition::any()
                        .add(jobs::Column::LeaseExpiresAt.is_null())
                        .add(jobs::Column::LeaseExpiresAt.lte(expired_before.to_string())),
                );
            }
            running
        } else {
            update.filter(jobs::Column::State.eq(JobState::Pending.as_str()))
        };
        let res = update.exec(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(res.rows_affected == 1)
    }
}

/// Serializes admission and quota updates for the current transaction.
///
/// PostgreSQL uses an advisory transaction lock so `COUNT` then `INSERT` is
/// safe under `READ COMMITTED`. SQLite and D1 take a write lock on the
/// singleton `job_queue_control` row.
///
/// # Errors
///
/// Returns [`LibraryError::Orm`] when the lock statement fails.
pub(crate) async fn lock_job_queue<C: ConnectionTrait>(db: &C) -> Result<()> {
    crate::sql_plan::lock_serialization_slot(db, crate::sql_plan::JOB_QUEUE_SLOT).await
}

/// Transactional admission used by the local path and guest atomic execute.
pub(crate) async fn enqueue_job_on<C: ConnectionTrait>(
    db: &C,
    spec: EnqueueJobSpec,
) -> Result<EnqueueOutcome> {
    lock_job_queue(db).await?;
    if spec.kind == JobKind::Invalid {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "cannot enqueue an invalid job kind"
        )));
    }
    if spec.payload.v != JOB_PAYLOAD_VERSION {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "unsupported job payload version {}",
            spec.payload.v
        )));
    }
    let dedup_key = spec.kind.dedup_key(&spec.payload);
    if let Some(existing) = find_active_job_by_dedup_on(db, &dedup_key).await? {
        return Ok(EnqueueOutcome::Duplicate {
            existing_id: existing.id,
        });
    }
    let active = count_active_jobs_on(db).await?;
    if active >= spec.max_pending.max(0) {
        return Ok(EnqueueOutcome::QueueFull);
    }
    let now = Utc::now();
    let run_after = spec.run_after.unwrap_or(now);
    let id = format!("{}-{}", spec.kind.as_str(), Uuid::new_v4());
    let payload = serde_json::to_string(&spec.payload)
        .unwrap_or_else(|_| serde_json::json!({"v": JOB_PAYLOAD_VERSION}).to_string());
    let now_s = now.to_rfc3339();
    let am = jobs::ActiveModel {
        id: Set(id.clone()),
        kind: Set(spec.kind.as_str().to_string()),
        state: Set(JobState::Pending.as_str().to_string()),
        priority: Set(spec.priority),
        resource_class: Set(spec.kind.resource_class().as_str().to_string()),
        payload: Set(payload),
        progress: Set(None),
        attempt_count: Set(0),
        max_attempts: Set(spec.max_attempts.max(1)),
        run_after: Set(run_after.to_rfc3339()),
        lease_owner: Set(None),
        lease_expires_at: Set(None),
        dedup_key: Set(dedup_key.clone()),
        error_kind: Set(None),
        error_message: Set(None),
        cancel_requested: Set(0),
        created_at: Set(now_s.clone()),
        updated_at: Set(now_s),
        started_at: Set(None),
        finished_at: Set(None),
        lease_generation: Set(0),
    };
    match am.insert(db).await {
        Ok(_) => Ok(EnqueueOutcome::Created { id }),
        Err(err) if is_unique_violation(&err) => {
            if let Some(existing) = find_active_job_by_dedup_on(db, &dedup_key).await? {
                return Ok(EnqueueOutcome::Duplicate {
                    existing_id: existing.id,
                });
            }
            Err(LibraryError::Orm(err))
        }
        Err(err) => Err(LibraryError::Orm(err)),
    }
}

/// Transactional claim: one conditional `pending` → `running` mutation.
pub(crate) async fn claim_next_job_on<C: ConnectionTrait>(
    db: &C,
    resource_class: JobResourceClass,
    owner: &str,
    lease_secs: u64,
) -> Result<Option<JobRecord>> {
    let now = Utc::now();
    let now_s = now.to_rfc3339();
    sanitize_unreadable_pending_on(db, &now_s).await?;
    let candidates = jobs::Entity::find()
        .filter(jobs::Column::ResourceClass.eq(resource_class.as_str()))
        .filter(jobs::Column::State.eq(JobState::Pending.as_str()))
        .filter(jobs::Column::RunAfter.lte(now_s.clone()))
        .order_by_desc(jobs::Column::Priority)
        .order_by_asc(jobs::Column::CreatedAt)
        .limit(8)
        .all(db)
        .await
        .map_err(LibraryError::Orm)?;
    let lease_expires =
        (now + Duration::seconds(i64::try_from(lease_secs).unwrap_or(60))).to_rfc3339();
    for model in candidates {
        if model.cancel_requested != 0 {
            let _ = jobs::Entity::update_many()
                .col_expr(
                    jobs::Column::State,
                    sea_orm::sea_query::Expr::value(JobState::Cancelled.as_str()),
                )
                .col_expr(
                    jobs::Column::CancelRequested,
                    sea_orm::sea_query::Expr::value(1i64),
                )
                .col_expr(
                    jobs::Column::FinishedAt,
                    sea_orm::sea_query::Expr::value(Some(now_s.clone())),
                )
                .col_expr(
                    jobs::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(now_s.clone()),
                )
                .filter(jobs::Column::Id.eq(&model.id))
                .filter(jobs::Column::State.eq(JobState::Pending.as_str()))
                .exec(db)
                .await
                .map_err(LibraryError::Orm)?;
            continue;
        }
        if let Err(reason) = try_map_job(model.clone()) {
            mark_pending_job_invalid_on(db, &model.id, &reason, &now_s).await?;
            continue;
        }
        let mut payload: JobPayload = serde_json::from_str(&model.payload).unwrap_or_default();
        let resuming = payload.resume_pending;
        let attempt = if resuming {
            model.attempt_count.max(1)
        } else {
            model.attempt_count + 1
        };
        let payload_json = if resuming {
            payload.resume_pending = false;
            Some(
                serde_json::to_string(&payload)
                    .map_err(|err| LibraryError::Other(anyhow::anyhow!("job payload: {err}")))?,
            )
        } else {
            None
        };
        let generation = model.lease_generation + 1;
        let started = model.started_at.clone().unwrap_or_else(|| now_s.clone());
        let mut update = jobs::Entity::update_many()
            .col_expr(
                jobs::Column::State,
                sea_orm::sea_query::Expr::value(JobState::Running.as_str()),
            )
            .col_expr(
                jobs::Column::AttemptCount,
                sea_orm::sea_query::Expr::value(attempt),
            )
            .col_expr(
                jobs::Column::LeaseOwner,
                sea_orm::sea_query::Expr::value(Some(owner.to_string())),
            )
            .col_expr(
                jobs::Column::LeaseExpiresAt,
                sea_orm::sea_query::Expr::value(Some(lease_expires.clone())),
            )
            .col_expr(
                jobs::Column::LeaseGeneration,
                sea_orm::sea_query::Expr::value(generation),
            )
            .col_expr(
                jobs::Column::StartedAt,
                sea_orm::sea_query::Expr::value(Some(started)),
            )
            .col_expr(
                jobs::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now_s.clone()),
            )
            .col_expr(
                jobs::Column::ErrorKind,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                jobs::Column::ErrorMessage,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            );
        if let Some(payload_json) = payload_json {
            update = update.col_expr(
                jobs::Column::Payload,
                sea_orm::sea_query::Expr::value(payload_json),
            );
        }
        let res = update
            .filter(jobs::Column::Id.eq(&model.id))
            .filter(jobs::Column::State.eq(JobState::Pending.as_str()))
            .exec(db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected != 1 {
            continue;
        }
        let Some(updated) = jobs::Entity::find_by_id(&model.id)
            .one(db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            continue;
        };
        return Ok(Some(try_map_job(updated).map_err(|reason| {
            LibraryError::Other(anyhow::anyhow!("claimed unreadable job: {reason}"))
        })?));
    }
    Ok(None)
}

/// Transactional quota reservation for one scratch path.
pub(crate) async fn reserve_job_temp_path_on<C: ConnectionTrait>(
    db: &C,
    job_id: &str,
    path: &str,
    reserved_bytes: u64,
    quota_bytes: u64,
) -> Result<()> {
    lock_job_queue(db).await?;
    let rows = job_temp_paths::Entity::find()
        .all(db)
        .await
        .map_err(LibraryError::Orm)?;
    let existing = rows.iter().find(|r| r.job_id == job_id && r.path == path);
    let already = existing
        .map(|r| u64::try_from(r.reserved_bytes).unwrap_or(0))
        .unwrap_or(0);
    let used: u64 = rows
        .iter()
        .map(|r| u64::try_from(r.reserved_bytes).unwrap_or(0))
        .fold(0u64, u64::saturating_add);
    let next_used = used.saturating_sub(already).saturating_add(reserved_bytes);
    if next_used > quota_bytes {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "acquire scratch quota exceeded ({next_used} > {quota_bytes} bytes)"
        )));
    }
    if let Some(existing) = existing {
        let mut am: job_temp_paths::ActiveModel = existing.clone().into();
        am.reserved_bytes = Set(i64::try_from(reserved_bytes).unwrap_or(i64::MAX));
        am.update(db).await.map_err(LibraryError::Orm)?;
        return Ok(());
    }
    let am = job_temp_paths::ActiveModel {
        id: NotSet,
        job_id: Set(job_id.to_string()),
        path: Set(path.to_string()),
        created_at: Set(now_str()),
        reserved_bytes: Set(i64::try_from(reserved_bytes).unwrap_or(i64::MAX)),
    };
    am.insert(db).await.map_err(LibraryError::Orm)?;
    Ok(())
}

/// Loads the oldest pending/running row with `dedup_key`, if any.
async fn find_active_job_by_dedup_on<C: ConnectionTrait>(
    db: &C,
    dedup_key: &str,
) -> Result<Option<JobRecord>> {
    let Some(model) = jobs::Entity::find()
        .filter(jobs::Column::DedupKey.eq(dedup_key))
        .filter(jobs::Column::State.is_in([JobState::Pending.as_str(), JobState::Running.as_str()]))
        .order_by_asc(jobs::Column::CreatedAt)
        .one(db)
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    Ok(Some(try_map_job(model).map_err(|reason| {
        LibraryError::Other(anyhow::anyhow!("unreadable active job: {reason}"))
    })?))
}

/// Counts pending + running jobs.
async fn count_active_jobs_on<C: ConnectionTrait>(db: &C) -> Result<i64> {
    let n = jobs::Entity::find()
        .filter(jobs::Column::State.is_in([JobState::Pending.as_str(), JobState::Running.as_str()]))
        .count(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

/// True when `err` is a unique-index conflict (SQLite or Postgres).
fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    let s = err.to_string();
    s.contains("UNIQUE") || s.contains("unique") || s.contains("23505")
}

/// Rewrites pending rows with an unknown `resource_class` so they cannot
/// occupy the admission cap forever (class-specific claim never sees them).
async fn sanitize_unreadable_pending_on<C: ConnectionTrait>(db: &C, now_s: &str) -> Result<()> {
    let suspects = jobs::Entity::find()
        .filter(jobs::Column::State.eq(JobState::Pending.as_str()))
        .filter(jobs::Column::ResourceClass.is_not_in(JobResourceClass::ALL.iter().copied()))
        .limit(32)
        .all(db)
        .await
        .map_err(LibraryError::Orm)?;
    for model in suspects {
        mark_pending_job_invalid_on(
            db,
            &model.id,
            &format!("unknown job resource class `{}`", model.resource_class),
            now_s,
        )
        .await?;
    }
    Ok(())
}

/// Marks one pending row `invalid_job` so it cannot be claimed.
async fn mark_pending_job_invalid_on<C: ConnectionTrait>(
    db: &C,
    id: &str,
    reason: &str,
    now_s: &str,
) -> Result<()> {
    let _ = jobs::Entity::update_many()
        .col_expr(
            jobs::Column::Kind,
            sea_orm::sea_query::Expr::value(JobKind::Invalid.as_str()),
        )
        .col_expr(
            jobs::Column::ResourceClass,
            sea_orm::sea_query::Expr::value(JobResourceClass::Network.as_str()),
        )
        .col_expr(
            jobs::Column::Payload,
            sea_orm::sea_query::Expr::value(invalid_job_payload_json()),
        )
        .col_expr(
            jobs::Column::State,
            sea_orm::sea_query::Expr::value(JobState::Failed.as_str()),
        )
        .col_expr(
            jobs::Column::ErrorKind,
            sea_orm::sea_query::Expr::value(Some("invalid_job".to_string())),
        )
        .col_expr(
            jobs::Column::ErrorMessage,
            sea_orm::sea_query::Expr::value(Some(reason.to_string())),
        )
        .col_expr(
            jobs::Column::FinishedAt,
            sea_orm::sea_query::Expr::value(Some(now_s.to_string())),
        )
        .col_expr(
            jobs::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(now_s.to_string()),
        )
        .filter(jobs::Column::Id.eq(id))
        .filter(jobs::Column::State.eq(JobState::Pending.as_str()))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Valid placeholder envelope written onto `invalid_job` rows.
fn invalid_job_payload_json() -> String {
    serde_json::to_string(&JobPayload::default())
        .unwrap_or_else(|_| format!(r#"{{"v":{JOB_PAYLOAD_VERSION}}}"#))
}

/// Maps a `jobs` row to [`JobRecord`], failing closed on unknown commands.
fn try_map_job(m: jobs::Model) -> std::result::Result<JobRecord, String> {
    let kind = JobKind::parse(&m.kind).ok_or_else(|| format!("unknown job kind `{}`", m.kind))?;
    let state =
        JobState::parse(&m.state).ok_or_else(|| format!("unknown job state `{}`", m.state))?;
    let resource_class = JobResourceClass::parse(&m.resource_class)
        .ok_or_else(|| format!("unknown job resource class `{}`", m.resource_class))?;
    let payload: JobPayload = serde_json::from_str(&m.payload)
        .map_err(|err| format!("malformed job payload JSON: {err}"))?;
    if payload.v != JOB_PAYLOAD_VERSION {
        return Err(format!(
            "unsupported job payload version {} (expected {JOB_PAYLOAD_VERSION})",
            payload.v
        ));
    }
    if JobTrigger::parse(payload.trigger.as_str()).is_none() {
        return Err(format!(
            "unknown job trigger `{}`",
            payload.trigger.as_str()
        ));
    }
    Ok(JobRecord {
        id: m.id,
        kind,
        state,
        priority: m.priority,
        resource_class,
        payload,
        progress: m.progress,
        attempt_count: m.attempt_count,
        max_attempts: m.max_attempts,
        run_after: parse_dt(&m.run_after),
        lease_owner: m.lease_owner,
        lease_expires_at: parse_dt_opt(m.lease_expires_at.as_deref()),
        dedup_key: m.dedup_key,
        error_kind: m.error_kind,
        error_message: m.error_message,
        cancel_requested: m.cancel_requested != 0,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
        started_at: parse_dt_opt(m.started_at.as_deref()),
        finished_at: parse_dt_opt(m.finished_at.as_deref()),
        lease_generation: m.lease_generation,
    })
}

/// Maps a `job_temp_paths` row to [`JobTempPath`].
fn map_temp_path(m: job_temp_paths::Model) -> JobTempPath {
    JobTempPath {
        id: m.id,
        job_id: m.job_id,
        path: m.path,
        created_at: parse_dt(&m.created_at),
        reserved_bytes: u64::try_from(m.reserved_bytes).unwrap_or(0),
    }
}

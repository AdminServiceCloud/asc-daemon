//! gRPC transport for BackupService and ScheduleService (DMN-114/DMN-115):
//! proto ↔ domain conversions around the shared [`ApiState`] methods in
//! [`super::backups`].

use std::collections::BTreeMap;

use tonic::{Request, Response, Status};

use super::backups::{BackupListing, JobView, StorageRow};
use super::grpc::{Grpc, ctx_of, to_status};
use super::proto::v1 as pb;
use crate::daemon::backup::storage::StorageKind;
use crate::daemon::scheduler::jobs;

use pb::backup_service_server::BackupService;
use pb::schedule_service_server::ScheduleService;

fn storage_to_pb(row: StorageRow) -> pb::BackupStorage {
    pb::BackupStorage {
        name: row.name,
        r#type: row.kind.to_string(),
        builtin: row.builtin,
        managed_by: row.managed_by,
        dir: row.dir,
        s3_endpoint: row.s3_endpoint,
        s3_bucket: row.s3_bucket,
        s3_region: row.s3_region,
        s3_prefix: row.s3_prefix,
        host: row.host,
        port: row.port,
    }
}

fn listing_to_pb(listing: BackupListing) -> pb::ListBackupsResponse {
    pb::ListBackupsResponse {
        backups: listing
            .backups
            .into_iter()
            .map(|b| pb::BackupEntry {
                app_id: b.app_id,
                storage: b.storage,
                name: b.name,
                size_bytes: b.size_bytes,
                created_unix: b.created_unix,
            })
            .collect(),
        errors: listing
            .errors
            .into_iter()
            .map(|e| pb::BackupStorageError {
                storage: e.storage,
                error: e.error,
            })
            .collect(),
    }
}

fn run_to_pb(run: jobs::RunRecord) -> pb::ScheduleRunRecord {
    pb::ScheduleRunRecord {
        id: run.id,
        state: match run.status {
            jobs::RunStatus::Running => pb::ScheduleRunState::Running,
            jobs::RunStatus::Succeeded => pb::ScheduleRunState::Succeeded,
            jobs::RunStatus::Failed => pb::ScheduleRunState::Failed,
        } as i32,
        trigger: match run.trigger {
            jobs::RunTrigger::Schedule => "schedule",
            jobs::RunTrigger::Manual => "manual",
        }
        .to_string(),
        started_unix: run.started_at,
        finished_unix: run.finished_at.unwrap_or(0),
        error: run.error,
        output: run.output,
        http_status: run.http_status.unwrap_or(0) as u32,
    }
}

fn timeout(seconds: u32) -> Option<u32> {
    (seconds > 0).then_some(seconds)
}

fn job_to_pb(view: JobView) -> pb::ScheduleJob {
    use pb::schedule_job::Action;
    let job = view.job;
    let app = |app: String| pb::ScheduleAppTarget { app_id: app };
    let action = match job.action {
        jobs::JobAction::NodeReboot => Action::NodeReboot(pb::ScheduleNodeReboot {}),
        jobs::JobAction::AppStart { app: a } => Action::AppStart(app(a)),
        jobs::JobAction::AppStop { app: a } => Action::AppStop(app(a)),
        jobs::JobAction::AppRestart { app: a } => Action::AppRestart(app(a)),
        jobs::JobAction::AppUpdate { app: a } => Action::AppUpdate(app(a)),
        jobs::JobAction::Backup {
            app,
            storages,
            keep,
        } => Action::Backup(pb::ScheduleBackupJob {
            app_id: app,
            storages,
            keep,
        }),
        jobs::JobAction::Shell {
            command,
            app,
            timeout_secs,
        } => Action::Shell(pb::ScheduleShellJob {
            command,
            app_id: app,
            timeout_seconds: timeout_secs.unwrap_or(0),
        }),
        jobs::JobAction::Http {
            method,
            url,
            headers,
            body,
            timeout_secs,
        } => Action::Http(pb::ScheduleHttpJob {
            method,
            url,
            headers: headers.into_iter().collect(),
            body,
            timeout_seconds: timeout_secs.unwrap_or(0),
        }),
    };
    pb::ScheduleJob {
        id: job.id,
        trigger: job.trigger,
        utc: job.utc,
        enabled: job.enabled,
        comment: job.comment,
        managed_by: job.managed_by,
        action: Some(action),
        last_run: view.last_run.map(run_to_pb),
        next_run_unix: view.next_run_unix,
        created_unix: job.created_at,
        updated_unix: job.updated_at,
    }
}

#[allow(clippy::result_large_err)]
fn job_from_pb(job: pb::ScheduleJob) -> Result<jobs::Job, Status> {
    use pb::schedule_job::Action;
    let action = match job
        .action
        .ok_or_else(|| Status::invalid_argument("schedule action is required"))?
    {
        Action::NodeReboot(_) => jobs::JobAction::NodeReboot,
        Action::AppStart(t) => jobs::JobAction::AppStart { app: t.app_id },
        Action::AppStop(t) => jobs::JobAction::AppStop { app: t.app_id },
        Action::AppRestart(t) => jobs::JobAction::AppRestart { app: t.app_id },
        Action::AppUpdate(t) => jobs::JobAction::AppUpdate { app: t.app_id },
        Action::Backup(b) => jobs::JobAction::Backup {
            app: b.app_id,
            storages: b.storages,
            keep: b.keep,
        },
        Action::Shell(s) => jobs::JobAction::Shell {
            command: s.command,
            app: s.app_id.filter(|a| !a.is_empty()),
            timeout_secs: timeout(s.timeout_seconds),
        },
        Action::Http(h) => jobs::JobAction::Http {
            method: if h.method.is_empty() {
                "GET".into()
            } else {
                h.method
            },
            url: h.url,
            headers: h.headers.into_iter().collect::<BTreeMap<_, _>>(),
            body: h.body,
            timeout_secs: timeout(h.timeout_seconds),
        },
    };
    Ok(jobs::Job {
        id: job.id,
        trigger: job.trigger,
        utc: job.utc,
        enabled: job.enabled,
        comment: job.comment,
        managed_by: job.managed_by.filter(|m| !m.is_empty()),
        action,
        created_at: 0,
        updated_at: 0,
    })
}

/// Validation failures of a job or storage are the caller's fault.
fn invalid_or_status(err: anyhow::Error) -> Status {
    let msg = format!("{err:#}");
    if msg.contains("must be")
        || msg.contains("is empty")
        || msg.contains("is required")
        || msg.contains("invalid")
        || msg.contains("unsupported")
        || msg.contains("refusing")
        || msg.contains("already used")
        || msg.contains("duplicate")
        || msg.contains("is not a backup")
        || msg.contains("is not http")
        || msg.contains("no bucket")
    {
        Status::invalid_argument(msg)
    } else {
        to_status(err)
    }
}

#[tonic::async_trait]
impl BackupService for Grpc {
    async fn list_backup_storages(
        &self,
        _request: Request<pb::ListBackupStoragesRequest>,
    ) -> Result<Response<pb::ListBackupStoragesResponse>, Status> {
        let rows = self.0.backup_storages().await.map_err(to_status)?;
        Ok(Response::new(pb::ListBackupStoragesResponse {
            storages: rows.into_iter().map(storage_to_pb).collect(),
        }))
    }

    async fn upsert_backup_storage(
        &self,
        request: Request<pb::UpsertBackupStorageRequest>,
    ) -> Result<Response<pb::UpsertBackupStorageResponse>, Status> {
        let req = request.into_inner();
        let kind = match req.kind {
            Some(pb::upsert_backup_storage_request::Kind::S3(s3)) => StorageKind::S3 {
                bucket: s3.bucket,
                region: s3.region,
                endpoint: Some(s3.endpoint).filter(|e| !e.is_empty()),
                access_key: s3.access_key,
                secret_key: s3.secret_key,
                prefix: Some(s3.prefix).filter(|p| !p.is_empty()),
            },
            Some(pb::upsert_backup_storage_request::Kind::Local(local)) => {
                if local.dir.trim().is_empty() {
                    return Err(Status::invalid_argument("local storage dir is required"));
                }
                StorageKind::Local {
                    dir: local.dir.into(),
                }
            }
            None => return Err(Status::invalid_argument("storage kind is required")),
        };
        let row = self
            .0
            .backup_storage_upsert(req.name, req.managed_by, kind)
            .await
            .map_err(invalid_or_status)?;
        Ok(Response::new(pb::UpsertBackupStorageResponse {
            storage: Some(storage_to_pb(row)),
        }))
    }

    async fn remove_backup_storage(
        &self,
        request: Request<pb::RemoveBackupStorageRequest>,
    ) -> Result<Response<pb::RemoveBackupStorageResponse>, Status> {
        let req = request.into_inner();
        self.0
            .backup_storage_remove(req.name, req.managed_by)
            .await
            .map_err(invalid_or_status)?;
        Ok(Response::new(pb::RemoveBackupStorageResponse {}))
    }

    async fn list_backups(
        &self,
        request: Request<pb::ListBackupsRequest>,
    ) -> Result<Response<pb::ListBackupsResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let listing = self
            .0
            .backup_list(ctx, req.app_id, req.storage)
            .await
            .map_err(to_status)?;
        Ok(Response::new(listing_to_pb(listing)))
    }

    async fn create_backup(
        &self,
        request: Request<pb::CreateBackupRequest>,
    ) -> Result<Response<pb::CreateBackupResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let results = self
            .0
            .backup_create(ctx, req.app_id, req.storages, req.keep)
            .await
            .map_err(invalid_or_status)?;
        Ok(Response::new(pb::CreateBackupResponse {
            results: results
                .into_iter()
                .map(|r| pb::BackupResult {
                    storage: r.storage,
                    name: r.name,
                    size_bytes: r.size_bytes,
                    error: r.error,
                })
                .collect(),
        }))
    }

    async fn restore_backup(
        &self,
        request: Request<pb::RestoreBackupRequest>,
    ) -> Result<Response<pb::RestoreBackupResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let restarted = self
            .0
            .backup_restore(ctx, req.app_id, req.storage, req.name, req.stop_app)
            .await
            .map_err(|err| {
                let msg = format!("{err:#}");
                if msg.contains("must be stopped") {
                    Status::failed_precondition(msg)
                } else {
                    invalid_or_status(err)
                }
            })?;
        Ok(Response::new(pb::RestoreBackupResponse { restarted }))
    }

    async fn delete_backup(
        &self,
        request: Request<pb::DeleteBackupRequest>,
    ) -> Result<Response<pb::DeleteBackupResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        self.0
            .backup_delete(ctx, req.app_id, req.storage, req.name)
            .await
            .map_err(invalid_or_status)?;
        Ok(Response::new(pb::DeleteBackupResponse {}))
    }
}

#[tonic::async_trait]
impl ScheduleService for Grpc {
    async fn list_schedules(
        &self,
        request: Request<pb::ListSchedulesRequest>,
    ) -> Result<Response<pb::ListSchedulesResponse>, Status> {
        let req = request.into_inner();
        let views = self
            .0
            .schedules_list(req.managed_by.filter(|m| !m.is_empty()))
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::ListSchedulesResponse {
            schedules: views.into_iter().map(job_to_pb).collect(),
        }))
    }

    async fn upsert_schedule(
        &self,
        request: Request<pb::UpsertScheduleRequest>,
    ) -> Result<Response<pb::UpsertScheduleResponse>, Status> {
        let job = request
            .into_inner()
            .schedule
            .ok_or_else(|| Status::invalid_argument("schedule is required"))?;
        let view = self
            .0
            .schedule_upsert(job_from_pb(job)?)
            .await
            .map_err(invalid_or_status)?;
        Ok(Response::new(pb::UpsertScheduleResponse {
            schedule: Some(job_to_pb(view)),
        }))
    }

    async fn remove_schedule(
        &self,
        request: Request<pb::RemoveScheduleRequest>,
    ) -> Result<Response<pb::RemoveScheduleResponse>, Status> {
        let removed = self
            .0
            .schedule_remove(request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::RemoveScheduleResponse { removed }))
    }

    async fn replace_managed_schedules(
        &self,
        request: Request<pb::ReplaceManagedSchedulesRequest>,
    ) -> Result<Response<pb::ReplaceManagedSchedulesResponse>, Status> {
        let req = request.into_inner();
        let incoming = req
            .schedules
            .into_iter()
            .map(job_from_pb)
            .collect::<Result<Vec<_>, _>>()?;
        let views = self
            .0
            .schedules_replace_managed(req.managed_by, incoming)
            .await
            .map_err(invalid_or_status)?;
        Ok(Response::new(pb::ReplaceManagedSchedulesResponse {
            schedules: views.into_iter().map(job_to_pb).collect(),
        }))
    }

    async fn run_schedule(
        &self,
        request: Request<pb::RunScheduleRequest>,
    ) -> Result<Response<pb::RunScheduleResponse>, Status> {
        let run = self
            .0
            .schedule_run(request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::RunScheduleResponse {
            started: run.is_some(),
            run: run.map(run_to_pb),
        }))
    }

    async fn list_schedule_runs(
        &self,
        request: Request<pb::ListScheduleRunsRequest>,
    ) -> Result<Response<pb::ListScheduleRunsResponse>, Status> {
        let req = request.into_inner();
        let runs = self
            .0
            .schedule_runs(req.id, req.limit as usize)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::ListScheduleRunsResponse {
            runs: runs.into_iter().map(run_to_pb).collect(),
        }))
    }
}

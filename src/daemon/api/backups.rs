//! Service layer for the backup and scheduled-job APIs (DMN-114/DMN-115),
//! shared by the gRPC and REST transports. Like the sources/credentials
//! section of [`super::ApiState`], storages act on the daemon process's own
//! scope (the root daemon: the system list in `/etc/asc`), while every app
//! reference goes through the caller's [`UserContext`] like any other app
//! operation.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde::Serialize;

use super::ApiState;
use crate::daemon::apps::{RuntimeState, UserContext};
use crate::daemon::backup::{self, storage};
use crate::daemon::scheduler::{self, jobs};

/// One storage as the API shows it — never with credentials.
#[derive(Debug, Clone, Serialize)]
pub struct StorageRow {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub builtin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
    pub dir: String,
    pub s3_endpoint: String,
    pub s3_bucket: String,
    pub s3_region: String,
    pub s3_prefix: String,
    pub host: String,
    pub port: u32,
}

impl StorageRow {
    fn builtin(dir: &std::path::Path) -> Self {
        Self {
            name: storage::LOCAL_NAME.to_string(),
            kind: "local",
            builtin: true,
            managed_by: None,
            dir: dir.display().to_string(),
            s3_endpoint: String::new(),
            s3_bucket: String::new(),
            s3_region: String::new(),
            s3_prefix: String::new(),
            host: String::new(),
            port: 0,
        }
    }

    fn from_entry(entry: &storage::StorageEntry) -> Self {
        let mut row = Self {
            name: entry.name.clone(),
            kind: entry.kind.label(),
            builtin: false,
            managed_by: entry.managed_by.clone(),
            dir: String::new(),
            s3_endpoint: String::new(),
            s3_bucket: String::new(),
            s3_region: String::new(),
            s3_prefix: String::new(),
            host: String::new(),
            port: 0,
        };
        match &entry.kind {
            storage::StorageKind::Local { dir } => row.dir = dir.display().to_string(),
            storage::StorageKind::S3 {
                bucket,
                region,
                endpoint,
                prefix,
                ..
            } => {
                row.s3_bucket = bucket.clone();
                row.s3_region = region.clone();
                row.s3_endpoint = endpoint.clone().unwrap_or_default();
                row.s3_prefix = prefix.clone().unwrap_or_default();
            }
            storage::StorageKind::Ftp {
                host, port, dir, ..
            }
            | storage::StorageKind::Sftp {
                host, port, dir, ..
            } => {
                row.host = host.clone();
                row.port = *port as u32;
                row.dir = dir.clone().unwrap_or_default();
            }
        }
        row
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupRow {
    pub app_id: String,
    pub storage: String,
    pub name: String,
    pub size_bytes: u64,
    pub created_unix: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageErrorRow {
    pub storage: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BackupListing {
    pub backups: Vec<BackupRow>,
    pub errors: Vec<StorageErrorRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupResultRow {
    pub storage: String,
    pub name: String,
    pub size_bytes: u64,
    pub error: String,
}

/// A job with its derived, read-only state.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub job: jobs::Job,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<jobs::RunRecord>,
    /// 0 — disabled or not within a year.
    pub next_run_unix: i64,
}

fn view(job: jobs::Job, last: Option<jobs::RunRecord>) -> JobView {
    let next_run_unix = jobs::next_run(&job, jobs::unix_now()).unwrap_or(0);
    JobView {
        job,
        last_run: last,
        next_run_unix,
    }
}

impl ApiState {
    // ── Backup storages ──

    pub async fn backup_storages(self: &Arc<Self>) -> Result<Vec<StorageRow>> {
        self.blocking(|s| {
            let list = storage::StorageList::load()?;
            let mut rows = vec![StorageRow::builtin(
                &s.config.daemon.data_dir.join("backups"),
            )];
            rows.extend(list.entries().into_iter().map(StorageRow::from_entry));
            Ok(rows)
        })
        .await
    }

    pub async fn backup_storage_upsert(
        self: &Arc<Self>,
        name: String,
        managed_by: Option<String>,
        kind: storage::StorageKind,
    ) -> Result<StorageRow> {
        self.blocking(move |_s| {
            let name = name.trim().to_string();
            if name.is_empty() || name.contains('/') {
                bail!("storage name '{name}' is invalid");
            }
            let mut list = storage::StorageList::load()?;
            let entry = storage::StorageEntry {
                name,
                managed_by: managed_by.filter(|m| !m.trim().is_empty()),
                kind,
            };
            // Constructing the client validates the configuration (bucket,
            // endpoint shape) before it is persisted.
            storage::open(&entry.kind)?;
            let row = StorageRow::from_entry(&entry);
            list.upsert(entry)?;
            list.save()?;
            Ok(row)
        })
        .await
    }

    pub async fn backup_storage_remove(
        self: &Arc<Self>,
        name: String,
        managed_by: Option<String>,
    ) -> Result<()> {
        self.blocking(move |_s| {
            let mut list = storage::StorageList::load()?;
            if let Some(expected) = managed_by.as_deref() {
                match list.get(&name) {
                    Some(entry) if entry.managed_by.as_deref() == Some(expected) => {}
                    Some(_) => bail!("storage '{name}' is not managed by {expected}"),
                    None => bail!("storage '{name}' not found"),
                }
            }
            list.remove(&name)?;
            list.save()
        })
        .await
    }

    // ── Backups ──

    /// Archives of one app (or all apps the caller can see) on one storage
    /// (or all of them), newest first. Unlistable storages are reported,
    /// not fatal.
    pub async fn backup_list(
        self: &Arc<Self>,
        ctx: UserContext,
        app: Option<String>,
        storage_name: Option<String>,
    ) -> Result<BackupListing> {
        self.blocking(move |s| {
            let list = storage::StorageList::load()?;
            let app_ids: Vec<String> = match app.as_deref().filter(|a| !a.is_empty()) {
                Some(app) => vec![s.manager.get_authorized(&ctx, app)?.id],
                // Straight from the store: `manager.list` would also probe
                // every app's runtime state, which a listing does not need.
                None => s
                    .manager
                    .store()
                    .list()?
                    .into_iter()
                    .filter(|meta| ctx.is_root || meta.owner.uid == ctx.uid)
                    .map(|meta| meta.id)
                    .collect(),
            };
            let storages = match storage_name.filter(|n| !n.is_empty()) {
                Some(name) => vec![name],
                None => list.names(),
            };
            let mut listing = BackupListing::default();
            for name in storages {
                let impl_ = match backup::resolve_storage(&s.config, &list, &name) {
                    Ok(impl_) => impl_,
                    Err(err) => {
                        listing.errors.push(StorageErrorRow {
                            storage: name,
                            error: format!("{err:#}"),
                        });
                        continue;
                    }
                };
                for app_id in &app_ids {
                    match impl_.list(app_id) {
                        Ok(objects) => {
                            listing
                                .backups
                                .extend(objects.into_iter().map(|o| BackupRow {
                                    app_id: app_id.clone(),
                                    storage: name.clone(),
                                    created_unix: o.created_unix().unwrap_or(0),
                                    size_bytes: o.size,
                                    name: o.name,
                                }))
                        }
                        Err(err) => {
                            // One error per storage is enough — the same
                            // unreachable bucket fails for every app.
                            listing.errors.push(StorageErrorRow {
                                storage: name.clone(),
                                error: format!("{err:#}"),
                            });
                            break;
                        }
                    }
                }
            }
            listing.backups.sort_by(|a, b| {
                b.created_unix
                    .cmp(&a.created_unix)
                    .then(a.name.cmp(&b.name))
            });
            Ok(listing)
        })
        .await
    }

    pub async fn backup_create(
        self: &Arc<Self>,
        ctx: UserContext,
        app: String,
        storages: Vec<String>,
        keep: Option<u32>,
    ) -> Result<Vec<BackupResultRow>> {
        self.blocking(move |s| {
            let meta = s.manager.get_authorized(&ctx, &app)?;
            let config_dir = s.manager.store().app_dir(&meta.id)?.join("config");
            let policy = crate::daemon::pkg::settings::SettingValues::load(&config_dir)?
                .backup_policy()?
                .unwrap_or_default();
            let targets: Vec<String> = if !storages.is_empty() {
                storages
            } else if !policy.storages.is_empty() {
                policy.storages.clone()
            } else {
                vec![storage::LOCAL_NAME.to_string()]
            };
            if keep == Some(0) {
                bail!("keep must be at least 1");
            }
            let list = storage::StorageList::load()?;
            let results = backup::create_backup_multi(
                &s.config,
                s.manager.store(),
                &meta,
                &list,
                &targets,
                keep.or(policy.keep),
            );
            Ok(results
                .into_iter()
                .map(|(name, result)| match result {
                    Ok(info) => BackupResultRow {
                        storage: name,
                        name: info.name,
                        size_bytes: info.bytes,
                        error: String::new(),
                    },
                    Err(err) => BackupResultRow {
                        storage: name,
                        name: String::new(),
                        size_bytes: 0,
                        error: format!("{err:#}"),
                    },
                })
                .collect())
        })
        .await
    }

    /// Restore; with `stop_app` a running app is stopped first and started
    /// again afterwards (also after a failed restore — the files are then
    /// whatever the partial extraction left, which is no worse running
    /// than stopped, and the error still reaches the caller).
    pub async fn backup_restore(
        self: &Arc<Self>,
        ctx: UserContext,
        app: String,
        storage_name: String,
        name: String,
        stop_app: bool,
    ) -> Result<bool> {
        self.blocking(move |s| {
            let status = s.manager.status(&ctx, &app)?;
            let running = status.state == RuntimeState::Running;
            if running && !stop_app {
                bail!(
                    "app '{}' must be stopped before restoring a backup",
                    status.meta.id
                );
            }
            if running {
                s.manager.stop(&ctx, &status.meta.id)?;
            }
            let list = storage::StorageList::load()?;
            let restored = backup::restore_backup(
                &s.config,
                s.manager.store(),
                &status.meta,
                &list,
                &storage_name,
                &name,
            );
            if running {
                s.manager.start(&ctx, &status.meta.id)?;
            }
            restored?;
            Ok(running)
        })
        .await
    }

    pub async fn backup_delete(
        self: &Arc<Self>,
        ctx: UserContext,
        app: String,
        storage_name: String,
        name: String,
    ) -> Result<()> {
        self.blocking(move |s| {
            let meta = s.manager.get_authorized(&ctx, &app)?;
            let list = storage::StorageList::load()?;
            backup::delete_backup(&s.config, &list, &storage_name, &meta.id, &name)
        })
        .await
    }

    // ── Scheduled jobs ──

    pub async fn schedules_list(
        self: &Arc<Self>,
        managed_by: Option<String>,
    ) -> Result<Vec<JobView>> {
        self.blocking(move |s| {
            let store = jobs::JobStore::for_config(&s.config);
            let mut last = store.last_runs()?;
            Ok(store
                .list()?
                .into_iter()
                .filter(|j| managed_by.is_none() || j.managed_by == managed_by)
                .map(|j| {
                    let run = last.remove(&j.id);
                    view(j, run)
                })
                .collect())
        })
        .await
    }

    pub async fn schedule_upsert(self: &Arc<Self>, job: jobs::Job) -> Result<JobView> {
        self.blocking(move |s| {
            let store = jobs::JobStore::for_config(&s.config);
            let saved = store.upsert(job)?;
            let last = store.last_runs()?.remove(&saved.id);
            Ok(view(saved, last))
        })
        .await
    }

    pub async fn schedule_remove(self: &Arc<Self>, id: String) -> Result<bool> {
        self.blocking(move |s| jobs::JobStore::for_config(&s.config).remove(&id))
            .await
    }

    pub async fn schedules_replace_managed(
        self: &Arc<Self>,
        managed_by: String,
        incoming: Vec<jobs::Job>,
    ) -> Result<Vec<JobView>> {
        self.blocking(move |s| {
            let store = jobs::JobStore::for_config(&s.config);
            let saved = store.replace_managed(&managed_by, incoming)?;
            let mut last = store.last_runs()?;
            Ok(saved
                .into_iter()
                .map(|j| {
                    let run = last.remove(&j.id);
                    view(j, run)
                })
                .collect())
        })
        .await
    }

    /// Start a job now; `None` when it is already running.
    pub async fn schedule_run(self: &Arc<Self>, id: String) -> Result<Option<jobs::RunRecord>> {
        self.blocking(move |s| {
            let job = jobs::JobStore::for_config(&s.config)
                .get(&id)?
                .ok_or_else(|| anyhow::anyhow!("schedule '{id}' not found"))?;
            scheduler::spawn_job(&s.config, job, jobs::RunTrigger::Manual)
        })
        .await
    }

    pub async fn schedule_runs(
        self: &Arc<Self>,
        id: String,
        limit: usize,
    ) -> Result<Vec<jobs::RunRecord>> {
        self.blocking(move |s| {
            let limit = if limit == 0 {
                jobs::MAX_RUNS_PER_JOB
            } else {
                limit
            };
            jobs::JobStore::for_config(&s.config).runs(&id, limit)
        })
        .await
    }
}

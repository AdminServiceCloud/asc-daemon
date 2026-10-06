//! Firewall API: the service layer over
//! [`crate::daemon::firewall::Firewall`] shared by both transports, the gRPC
//! `FirewallService` and the REST routes the CLI uses. Every call is
//! root-only: a non-root unix-socket peer is refused before anything runs.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use tonic::{Request, Response as GrpcResponse, Status};

use super::ApiState;
use super::grpc::{Grpc, ctx_of, to_status};
use super::proto::v1 as pb;
use super::rest::ApiError;
use crate::daemon::apps::UserContext;
use crate::daemon::firewall::host::{self, Facts};
use crate::daemon::firewall::model::{
    Action, IpSets, Mode, Policy, Protocol, Rule, Scope, SetEntry, SetName, Settings,
};
use crate::daemon::firewall::{DEFAULT_TIMEOUT, Firewall, Overview};
use crate::daemon::users;

use pb::firewall_service_server::FirewallService;

// ── Service layer ───────────────────────────────────────────────────────────

impl ApiState {
    /// Host facts the system modules (firewall, fail2ban) derive their
    /// presets and available features from.
    pub(super) fn host_facts(&self) -> Facts {
        Facts {
            ssh_ports: host::detect_ssh_ports(),
            api_port: host::api_port(&self.config.api.listen),
            web_installed: self.webserver.load_settings().mode
                != crate::daemon::webserver::model::Mode::None,
        }
    }

    async fn fw<T, F>(self: &Arc<Self>, ctx: &UserContext, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Arc<Firewall>, &Facts) -> Result<T> + Send + 'static,
    {
        users::require_root(ctx)?;
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let facts = state.host_facts();
            f(&state.firewall, &facts)
        })
        .await
        .context("firewall worker panicked")?
    }

    /// Installs nftables with progress lines on the channel; the last message
    /// is the result.
    fn fw_install_stream(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> tokio::sync::mpsc::Receiver<InstallEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            if let Err(err) = users::require_root(&ctx) {
                let _ = tx.blocking_send(InstallEvent::Done(Err(err.into())));
                return;
            }
            let mut progress = |line: &str| {
                let _ = tx.blocking_send(InstallEvent::Line(line.to_string()));
            };
            let result = state.firewall.install_nft(&mut progress).and_then(|()| {
                let facts = state.host_facts();
                state.firewall.overview(&facts).map(Box::new)
            });
            let _ = tx.blocking_send(InstallEvent::Done(result));
        });
        rx
    }
}

enum InstallEvent {
    Line(String),
    Done(Result<Box<Overview>>),
}

fn timeout_or_default(seconds: Option<u64>) -> u64 {
    seconds.unwrap_or(DEFAULT_TIMEOUT)
}

// ── Proto conversions ───────────────────────────────────────────────────────

fn mode_to_pb(mode: Mode) -> i32 {
    let value = match mode {
        Mode::Disabled => pb::FirewallMode::Disabled,
        Mode::Managed => pb::FirewallMode::Managed,
        Mode::Raw => pb::FirewallMode::Raw,
    };
    value as i32
}

fn policy_to_pb(policy: Policy) -> i32 {
    let value = match policy {
        Policy::Accept => pb::FirewallPolicy::Accept,
        Policy::Drop => pb::FirewallPolicy::Drop,
    };
    value as i32
}

fn policy_from_pb(value: i32) -> Policy {
    match pb::FirewallPolicy::try_from(value) {
        Ok(pb::FirewallPolicy::Accept) => Policy::Accept,
        // Unspecified means the safe default.
        _ => Policy::Drop,
    }
}

fn action_to_pb(action: Action) -> i32 {
    let value = match action {
        Action::Accept => pb::FirewallAction::Accept,
        Action::Drop => pb::FirewallAction::Drop,
        Action::Reject => pb::FirewallAction::Reject,
    };
    value as i32
}

fn action_from_pb(value: i32) -> Action {
    match pb::FirewallAction::try_from(value) {
        Ok(pb::FirewallAction::Drop) => Action::Drop,
        Ok(pb::FirewallAction::Reject) => Action::Reject,
        _ => Action::Accept,
    }
}

fn protocol_to_pb(protocol: Protocol) -> i32 {
    let value = match protocol {
        Protocol::Tcp => pb::FirewallProtocol::Tcp,
        Protocol::Udp => pb::FirewallProtocol::Udp,
        Protocol::Any => pb::FirewallProtocol::Any,
        Protocol::Icmp => pb::FirewallProtocol::Icmp,
    };
    value as i32
}

fn protocol_from_pb(value: i32) -> Protocol {
    match pb::FirewallProtocol::try_from(value) {
        Ok(pb::FirewallProtocol::Tcp) => Protocol::Tcp,
        Ok(pb::FirewallProtocol::Udp) => Protocol::Udp,
        Ok(pb::FirewallProtocol::Icmp) => Protocol::Icmp,
        _ => Protocol::Any,
    }
}

fn scope_to_pb(scope: Scope) -> i32 {
    let value = match scope {
        Scope::Host => pb::FirewallScope::Host,
        Scope::Docker => pb::FirewallScope::Docker,
        Scope::Both => pb::FirewallScope::Both,
    };
    value as i32
}

fn scope_from_pb(value: i32) -> Scope {
    match pb::FirewallScope::try_from(value) {
        Ok(pb::FirewallScope::Docker) => Scope::Docker,
        Ok(pb::FirewallScope::Both) => Scope::Both,
        _ => Scope::Host,
    }
}

fn settings_to_pb(s: &Settings) -> pb::FirewallSettings {
    pb::FirewallSettings {
        input_policy: policy_to_pb(s.input_policy),
        allow_icmp: s.allow_icmp,
        ipv6: s.ipv6,
        protect_docker: s.protect_docker,
        disabled_presets: s.disabled_presets.clone(),
    }
}

fn settings_from_pb(p: pb::FirewallSettings) -> Settings {
    Settings {
        input_policy: policy_from_pb(p.input_policy),
        allow_icmp: p.allow_icmp,
        ipv6: p.ipv6,
        protect_docker: p.protect_docker,
        disabled_presets: p.disabled_presets,
    }
}

fn rule_to_pb(
    rule: &Rule,
    counter: Option<&crate::daemon::firewall::nft::Counter>,
) -> pb::FirewallRule {
    pb::FirewallRule {
        id: rule.id.clone(),
        enabled: rule.enabled,
        action: action_to_pb(rule.action),
        protocol: protocol_to_pb(rule.protocol),
        ports: rule.ports.clone(),
        sources: rule.sources.clone(),
        scope: scope_to_pb(rule.scope),
        comment: rule.comment.clone(),
        managed_by: rule.managed_by.clone(),
        packets: counter.map_or(0, |c| c.packets),
        bytes: counter.map_or(0, |c| c.bytes),
    }
}

fn rule_from_pb(p: pb::FirewallRule) -> Rule {
    Rule {
        id: p.id,
        enabled: p.enabled,
        action: action_from_pb(p.action),
        protocol: protocol_from_pb(p.protocol),
        ports: p.ports,
        sources: p.sources,
        scope: scope_from_pb(p.scope),
        comment: p.comment,
        managed_by: p.managed_by,
    }
}

fn entry_to_pb(entry: &SetEntry) -> pb::FirewallSetEntry {
    pb::FirewallSetEntry {
        value: entry.value.clone(),
        expires_unix: entry.expires_unix.unwrap_or(0),
        comment: entry.comment.clone(),
    }
}

fn entry_from_pb(p: pb::FirewallSetEntry) -> SetEntry {
    SetEntry {
        value: p.value,
        expires_unix: (p.expires_unix > 0).then_some(p.expires_unix),
        comment: p.comment,
    }
}

fn entries_to_pb(entries: &[SetEntry]) -> Vec<pb::FirewallSetEntry> {
    entries.iter().map(entry_to_pb).collect()
}

fn state_to_pb(o: &Overview) -> pb::FirewallState {
    let IpSets {
        allowlist,
        blocklist,
    } = &o.sets;
    pb::FirewallState {
        nft_installed: o.nft_installed,
        nft_version: o.nft_version.clone(),
        mode: mode_to_pb(o.mode),
        settings: Some(settings_to_pb(&o.settings)),
        rules: o
            .rules
            .iter()
            .map(|r| rule_to_pb(r, o.counters.get(&r.id)))
            .collect(),
        presets: o
            .presets
            .iter()
            .map(|r| rule_to_pb(r, o.counters.get(&r.id)))
            .collect(),
        allowlist: entries_to_pb(allowlist),
        blocklist: entries_to_pb(blocklist),
        pending: o.pending.as_ref().map(|p| pb::FirewallPending {
            id: p.id.clone(),
            deadline_unix: p.deadline_unix,
            seconds_left: p.seconds_left,
            target_mode: mode_to_pb(p.target_mode),
        }),
        conflicts: o
            .conflicts
            .iter()
            .map(|c| pb::FirewallConflict {
                kind: c.kind.clone(),
                detail: c.detail.clone(),
            })
            .collect(),
        changed: o.changed,
        last_error: o.last_error.clone(),
        applied_unix: o.applied_unix,
    }
}

// ── gRPC ────────────────────────────────────────────────────────────────────

#[tonic::async_trait]
impl FirewallService for Grpc {
    type InstallNftablesStreamStream =
        Pin<Box<dyn Stream<Item = Result<pb::FirewallInstallEvent, Status>> + Send>>;

    async fn get_firewall(
        &self,
        request: Request<pb::GetFirewallRequest>,
    ) -> Result<GrpcResponse<pb::GetFirewallResponse>, Status> {
        let ctx = ctx_of(&request);
        let overview = self
            .0
            .fw(&ctx, |fw, facts| fw.overview(facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::GetFirewallResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn install_nftables_stream(
        &self,
        request: Request<pb::InstallNftablesRequest>,
    ) -> Result<GrpcResponse<Self::InstallNftablesStreamStream>, Status> {
        let ctx = ctx_of(&request);
        let rx = self.0.fw_install_stream(ctx);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            let event = match rx.recv().await? {
                InstallEvent::Line(line) => pb::firewall_install_event::Event::Log(line),
                InstallEvent::Done(Ok(overview)) => {
                    pb::firewall_install_event::Event::Done(state_to_pb(&overview))
                }
                InstallEvent::Done(Err(err)) => {
                    pb::firewall_install_event::Event::Error(format!("{err:#}"))
                }
            };
            Some((Ok(pb::FirewallInstallEvent { event: Some(event) }), rx))
        });
        Ok(GrpcResponse::new(Box::pin(stream)))
    }

    async fn update_firewall_settings(
        &self,
        request: Request<pb::UpdateFirewallSettingsRequest>,
    ) -> Result<GrpcResponse<pb::UpdateFirewallSettingsResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let settings = settings_from_pb(
            req.settings
                .ok_or_else(|| Status::invalid_argument("settings are required"))?,
        );
        let overview = self
            .0
            .fw(&ctx, move |fw, facts| {
                fw.update_settings(settings, req.force)?;
                fw.overview(facts)
            })
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UpdateFirewallSettingsResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn upsert_firewall_rule(
        &self,
        request: Request<pb::UpsertFirewallRuleRequest>,
    ) -> Result<GrpcResponse<pb::UpsertFirewallRuleResponse>, Status> {
        let ctx = ctx_of(&request);
        let rule = request
            .into_inner()
            .rule
            .ok_or_else(|| Status::invalid_argument("rule is required"))?;
        let rule = rule_from_pb(rule);
        let rule = self
            .0
            .fw(&ctx, move |fw, _| fw.upsert_rule(rule))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UpsertFirewallRuleResponse {
            rule: Some(rule_to_pb(&rule, None)),
        }))
    }

    async fn remove_firewall_rule(
        &self,
        request: Request<pb::RemoveFirewallRuleRequest>,
    ) -> Result<GrpcResponse<pb::RemoveFirewallRuleResponse>, Status> {
        let ctx = ctx_of(&request);
        let id = request.into_inner().id;
        let removed = self
            .0
            .fw(&ctx, move |fw, _| fw.remove_rule(&id))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::RemoveFirewallRuleResponse {
            removed,
        }))
    }

    async fn set_firewall_ip_set(
        &self,
        request: Request<pb::SetFirewallIpSetRequest>,
    ) -> Result<GrpcResponse<pb::SetFirewallIpSetResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let name =
            SetName::parse(&req.set).map_err(|err| Status::invalid_argument(err.to_string()))?;
        let entries: Vec<SetEntry> = req.entries.into_iter().map(entry_from_pb).collect();
        let entries = self
            .0
            .fw(&ctx, move |fw, _| fw.set_entries(name, entries))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::SetFirewallIpSetResponse {
            entries: entries_to_pb(&entries),
        }))
    }

    async fn render_firewall(
        &self,
        request: Request<pb::RenderFirewallRequest>,
    ) -> Result<GrpcResponse<pb::RenderFirewallResponse>, Status> {
        let ctx = ctx_of(&request);
        let rendered = self
            .0
            .fw(&ctx, |fw, facts| fw.render(facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::RenderFirewallResponse {
            proposed: rendered.proposed,
            applied: rendered.applied,
            changed: rendered.changed,
        }))
    }

    async fn apply_firewall(
        &self,
        request: Request<pb::ApplyFirewallRequest>,
    ) -> Result<GrpcResponse<pb::ApplyFirewallResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let timeout = timeout_or_default(req.timeout_seconds.map(u64::from));
        let overview = self
            .0
            .fw(&ctx, move |fw, facts| fw.apply(facts, timeout, req.force))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::ApplyFirewallResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn confirm_firewall(
        &self,
        request: Request<pb::ConfirmFirewallRequest>,
    ) -> Result<GrpcResponse<pb::ConfirmFirewallResponse>, Status> {
        let ctx = ctx_of(&request);
        let id = request.into_inner().id;
        let overview = self
            .0
            .fw(&ctx, move |fw, facts| fw.confirm(&id, facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::ConfirmFirewallResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn rollback_firewall(
        &self,
        request: Request<pb::RollbackFirewallRequest>,
    ) -> Result<GrpcResponse<pb::RollbackFirewallResponse>, Status> {
        let ctx = ctx_of(&request);
        let overview = self
            .0
            .fw(&ctx, |fw, facts| fw.rollback(facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::RollbackFirewallResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn disable_firewall(
        &self,
        request: Request<pb::DisableFirewallRequest>,
    ) -> Result<GrpcResponse<pb::DisableFirewallResponse>, Status> {
        let ctx = ctx_of(&request);
        let overview = self
            .0
            .fw(&ctx, |fw, facts| fw.disable(facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::DisableFirewallResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn get_firewall_ruleset(
        &self,
        request: Request<pb::GetFirewallRulesetRequest>,
    ) -> Result<GrpcResponse<pb::GetFirewallRulesetResponse>, Status> {
        let ctx = ctx_of(&request);
        let view = self
            .0
            .fw(&ctx, |fw, _| fw.ruleset())
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::GetFirewallRulesetResponse {
            live: view.live,
            raw: view.raw,
        }))
    }

    async fn apply_firewall_ruleset(
        &self,
        request: Request<pb::ApplyFirewallRulesetRequest>,
    ) -> Result<GrpcResponse<pb::ApplyFirewallRulesetResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let timeout = timeout_or_default(req.timeout_seconds.map(u64::from));
        let overview = self
            .0
            .fw(&ctx, move |fw, facts| {
                fw.apply_raw(facts, &req.ruleset, req.acknowledge_risk, timeout)
            })
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::ApplyFirewallRulesetResponse {
            firewall: Some(state_to_pb(&overview)),
        }))
    }

    async fn list_firewall_tables(
        &self,
        request: Request<pb::ListFirewallTablesRequest>,
    ) -> Result<GrpcResponse<pb::ListFirewallTablesResponse>, Status> {
        let ctx = ctx_of(&request);
        let tables = self
            .0
            .fw(&ctx, |fw, _| fw.tables())
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::ListFirewallTablesResponse {
            tables: tables
                .into_iter()
                .map(|t| pb::FirewallTable {
                    family: t.family,
                    name: t.name,
                    owner: t.owner,
                    text: t.text,
                    truncated: t.truncated,
                })
                .collect(),
        }))
    }
}

// ── REST (CLI) ──────────────────────────────────────────────────────────────

pub fn routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/v1/firewall", get(rest_get))
        .route("/v1/firewall/install", post(rest_install))
        .route("/v1/firewall/settings", put(rest_settings))
        .route("/v1/firewall/rules", post(rest_upsert_rule))
        .route(
            "/v1/firewall/rules/{id}",
            put(rest_put_rule).delete(rest_remove_rule),
        )
        .route("/v1/firewall/sets/{name}", put(rest_set_entries))
        .route(
            "/v1/firewall/sets/{name}/entries",
            post(rest_add_entry).delete(rest_remove_entry),
        )
        .route("/v1/firewall/render", get(rest_render))
        .route("/v1/firewall/apply", post(rest_apply))
        .route("/v1/firewall/confirm", post(rest_confirm))
        .route("/v1/firewall/rollback", post(rest_rollback))
        .route("/v1/firewall/disable", post(rest_disable))
        .route(
            "/v1/firewall/ruleset",
            get(rest_ruleset).put(rest_apply_ruleset),
        )
        .route("/v1/firewall/tables", get(rest_tables))
}

async fn rest_get(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let overview = state.fw(&ctx, |fw, facts| fw.overview(facts)).await?;
    Ok(Json(overview).into_response())
}

async fn rest_install(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let mut rx = state.fw_install_stream(ctx);
    let mut log = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            InstallEvent::Line(line) => log.push(line),
            InstallEvent::Done(result) => {
                let overview = result?;
                return Ok(
                    Json(serde_json::json!({ "log": log, "firewall": overview })).into_response(),
                );
            }
        }
    }
    Err(anyhow::anyhow!("the install worker stopped unexpectedly").into())
}

#[derive(Deserialize)]
struct SettingsBody {
    settings: Settings,
    #[serde(default)]
    force: bool,
}

async fn rest_settings(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<SettingsBody>,
) -> Result<Response, ApiError> {
    let overview = state
        .fw(&ctx, move |fw, facts| {
            fw.update_settings(body.settings, body.force)?;
            fw.overview(facts)
        })
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_upsert_rule(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(rule): Json<Rule>,
) -> Result<Response, ApiError> {
    let rule = state.fw(&ctx, move |fw, _| fw.upsert_rule(rule)).await?;
    Ok(Json(rule).into_response())
}

async fn rest_put_rule(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Json(mut rule): Json<Rule>,
) -> Result<Response, ApiError> {
    rule.id = id;
    let rule = state.fw(&ctx, move |fw, _| fw.upsert_rule(rule)).await?;
    Ok(Json(rule).into_response())
}

async fn rest_remove_rule(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let removed = state.fw(&ctx, move |fw, _| fw.remove_rule(&id)).await?;
    Ok(Json(serde_json::json!({ "removed": removed })).into_response())
}

#[derive(Deserialize)]
struct EntriesBody {
    entries: Vec<SetEntry>,
}

async fn rest_set_entries(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
    Json(body): Json<EntriesBody>,
) -> Result<Response, ApiError> {
    let name = SetName::parse(&name).map_err(crate::daemon::exec::invalid)?;
    let entries = state
        .fw(&ctx, move |fw, _| fw.set_entries(name, body.entries))
        .await?;
    Ok(Json(serde_json::json!({ "entries": entries })).into_response())
}

async fn rest_add_entry(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
    Json(entry): Json<SetEntry>,
) -> Result<Response, ApiError> {
    let name = SetName::parse(&name).map_err(crate::daemon::exec::invalid)?;
    let entries = state
        .fw(&ctx, move |fw, _| fw.add_entry(name, entry))
        .await?;
    Ok(Json(serde_json::json!({ "entries": entries })).into_response())
}

#[derive(Deserialize)]
struct EntryQuery {
    value: String,
}

async fn rest_remove_entry(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
    Query(query): Query<EntryQuery>,
) -> Result<Response, ApiError> {
    let name = SetName::parse(&name).map_err(crate::daemon::exec::invalid)?;
    let removed = state
        .fw(&ctx, move |fw, _| fw.remove_entry(name, &query.value))
        .await?;
    Ok(Json(serde_json::json!({ "removed": removed })).into_response())
}

async fn rest_render(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let rendered = state.fw(&ctx, |fw, facts| fw.render(facts)).await?;
    Ok(Json(rendered).into_response())
}

#[derive(Deserialize, Default)]
struct ApplyBody {
    timeout_seconds: Option<u64>,
    #[serde(default)]
    force: bool,
}

async fn rest_apply(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    body: Option<Json<ApplyBody>>,
) -> Result<Response, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let timeout = timeout_or_default(body.timeout_seconds);
    let overview = state
        .fw(&ctx, move |fw, facts| fw.apply(facts, timeout, body.force))
        .await?;
    Ok(Json(overview).into_response())
}

#[derive(Deserialize, Default)]
struct ConfirmBody {
    #[serde(default)]
    id: String,
}

async fn rest_confirm(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    body: Option<Json<ConfirmBody>>,
) -> Result<Response, ApiError> {
    let id = body.map(|Json(b)| b.id).unwrap_or_default();
    let overview = state
        .fw(&ctx, move |fw, facts| fw.confirm(&id, facts))
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_rollback(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let overview = state.fw(&ctx, |fw, facts| fw.rollback(facts)).await?;
    Ok(Json(overview).into_response())
}

async fn rest_disable(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let overview = state.fw(&ctx, |fw, facts| fw.disable(facts)).await?;
    Ok(Json(overview).into_response())
}

async fn rest_ruleset(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let view = state.fw(&ctx, |fw, _| fw.ruleset()).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
struct RulesetBody {
    ruleset: String,
    #[serde(default)]
    acknowledge_risk: bool,
    timeout_seconds: Option<u64>,
}

async fn rest_apply_ruleset(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<RulesetBody>,
) -> Result<Response, ApiError> {
    let timeout = timeout_or_default(body.timeout_seconds);
    let overview = state
        .fw(&ctx, move |fw, facts| {
            fw.apply_raw(facts, &body.ruleset, body.acknowledge_risk, timeout)
        })
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_tables(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let tables = state.fw(&ctx, |fw, _| fw.tables()).await?;
    Ok(Json(serde_json::json!({ "tables": tables })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_round_trip_through_proto() {
        let rule = Rule {
            id: "r-1".into(),
            enabled: false,
            action: Action::Reject,
            protocol: Protocol::Udp,
            ports: vec!["53".into()],
            sources: vec!["10.0.0.0/8".into()],
            scope: Scope::Both,
            comment: "dns".into(),
            managed_by: "user".into(),
        };
        assert_eq!(rule_from_pb(rule_to_pb(&rule, None)), rule);
    }

    #[test]
    fn unspecified_enums_take_the_safe_defaults() {
        assert_eq!(policy_from_pb(0), Policy::Drop);
        assert_eq!(protocol_from_pb(0), Protocol::Any);
        assert_eq!(scope_from_pb(0), Scope::Host);
    }

    #[test]
    fn permanent_entries_use_zero_on_the_wire() {
        let entry = SetEntry {
            value: "1.2.3.4".into(),
            expires_unix: None,
            comment: "x".into(),
        };
        let pb = entry_to_pb(&entry);
        assert_eq!(pb.expires_unix, 0);
        assert_eq!(entry_from_pb(pb), entry);
        let temp = SetEntry {
            expires_unix: Some(99),
            ..entry
        };
        assert_eq!(entry_from_pb(entry_to_pb(&temp)), temp);
    }

    #[test]
    fn the_default_window_applies_only_when_unset() {
        assert_eq!(timeout_or_default(None), DEFAULT_TIMEOUT);
        assert_eq!(timeout_or_default(Some(0)), 0);
        assert_eq!(timeout_or_default(Some(120)), 120);
    }
}

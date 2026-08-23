#[path = "../agent.rs"]
mod agent;
#[path = "../app_dirs.rs"]
mod app_dirs;
#[path = "../apply_patch.rs"]
mod apply_patch;
#[path = "../codex_app_server.rs"]
mod codex_app_server;
#[path = "../config.rs"]
mod config;
#[path = "../desktop.rs"]
mod desktop;
#[cfg(target_os = "linux")]
#[path = "../desktop_a11y.rs"]
mod desktop_a11y;
#[path = "../embeddings.rs"]
mod embeddings;
#[path = "../firecrawl.rs"]
mod firecrawl;
#[path = "../gui.rs"]
mod gui;
#[path = "../llama.rs"]
mod llama;
#[path = "../memory.rs"]
mod memory;
#[path = "../memory_engine.rs"]
mod memory_engine;
#[path = "../node_protocol.rs"]
mod node_protocol;
#[path = "../nodes.rs"]
mod nodes;
#[path = "../openrouter.rs"]
mod openrouter;
#[path = "../privilege.rs"]
mod privilege;
#[path = "../protocol.rs"]
mod protocol;
#[path = "../provider.rs"]
mod provider;
#[path = "../provider_catalog.rs"]
mod provider_catalog;
#[cfg(target_os = "linux")]
#[path = "../sandbox.rs"]
mod sandbox;
#[cfg(target_os = "macos")]
#[path = "../sandbox_macos.rs"]
mod sandbox;
#[path = "../skills.rs"]
mod skills;
#[path = "../storage.rs"]
mod storage;
#[path = "../store.rs"]
mod store;
#[path = "../tooling.rs"]
mod tooling;
#[path = "../coding_tools.rs"]
mod tools;
#[path = "../transcribe.rs"]
mod transcribe;
#[path = "../uploads.rs"]
mod uploads;
#[path = "../verify.rs"]
mod verify;
#[path = "../workspaces.rs"]
mod workspaces;

use agent::{Agent, ApprovalPolicy};
use anyhow::{Context, Result, bail};
use config::AppConfig;
use memory_engine::{DreamHandle, MemoryEngine, spawn_dream_worker};
use privilege::{PrivilegeBroker, PrivilegeCredential};
use protocol::{Decision, Event, HistoryTurn, Op, SessionSummary};
use provider_catalog::{ProviderSelection, ProviderSettingsStore, build_provider, preset};
use sandbox::{SandboxMode, SandboxPolicy};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use store::Store;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tooling::{Registry, ToolOutputStore};
use workspaces::{WorkspaceHistory, resolve_startup_workspace};

struct Cli {
    workspace: Option<PathBuf>,
    session: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    sandbox: SandboxMode,
}

struct ActiveTurn {
    cancel: CancellationToken,
    handle: JoinHandle<Result<()>>,
}

fn main() -> Result<()> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        bail!("gnomef-agent currently supports Linux and macOS");
    }

    // This must run before Tokio creates worker threads. The helper re-execs a
    // command only after applying Landlock and seccomp restrictions.
    sandbox::maybe_run_as_helper()?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let cli = parse_cli()?;
    let launch_dir = std::env::current_dir().context("cannot determine current directory")?;
    let app_home = app_dirs::resolve_app_home(&launch_dir)?;
    let config_path = app_home.join("config.json");
    let config = AppConfig::load(&config_path)?;
    // Save generated persistent secrets (notably node enrollment) before the
    // background Hub process loads the same configuration.
    config.save(&config_path)?;
    let store = Store::open(&app_home.join("store/agent.db"))?;
    let provider_settings = ProviderSettingsStore::new(app_home.join("store/providers.json"));
    let app_paths = storage::AppPaths::new(app_home.clone())?;
    let memory_engine = MemoryEngine::open(&app_paths)?;
    let mut workspace_history = WorkspaceHistory::load(app_home.join("store/workspaces.json"));

    let model_override = cli.model.or_else(|| non_empty_env("GNOMEF_MODEL"));
    let (startup_workspace, workspace_note) =
        resolve_startup_workspace(cli.workspace, &launch_dir, &workspace_history);
    let requested_workspace = Some(startup_workspace);
    let base_override = cli.base_url.or_else(|| non_empty_env("GNOMEF_BASE_URL"));
    let key_override = non_empty_env("GNOMEF_API_KEY");
    let explicit_legacy_provider = base_override.is_some() || key_override.is_some();
    let saved_provider = if explicit_legacy_provider {
        None
    } else {
        provider_settings.load()?
    };
    let provider_is_persisted = saved_provider.is_some();
    let mut provider_selection = if explicit_legacy_provider {
        ProviderSelection::legacy(
            base_override.unwrap_or_else(|| config.llama_base_url.clone()),
            key_override.or_else(|| {
                let key = config.llama_api_key.trim();
                (!key.is_empty()).then(|| key.to_string())
            }),
            model_override
                .clone()
                .unwrap_or_else(|| config.default_model.clone()),
        )
    } else if let Some(saved) = saved_provider {
        saved
    } else {
        ProviderSelection::legacy(
            config.llama_base_url.clone(),
            {
                let key = config.llama_api_key.trim();
                (!key.is_empty()).then(|| key.to_string())
            },
            config.default_model.clone(),
        )
    };

    let (session_id, workspace, mut model) = if let Some(id) = cli.session {
        let session = store
            .get_session(&id)?
            .with_context(|| format!("agent session `{id}` does not exist"))?;
        let saved_workspace = canonical_workspace(&session.workspace)?;
        if let Some(requested) = requested_workspace {
            let requested = canonical_workspace(&requested)?;
            if requested != saved_workspace {
                bail!(
                    "session `{id}` belongs to {}, not {}",
                    saved_workspace.display(),
                    requested.display()
                );
            }
        }
        let model = model_override.unwrap_or_else(|| {
            if provider_is_persisted {
                provider_selection.model.clone()
            } else {
                session.model
            }
        });
        store.set_model(&id, &model)?;
        (id, saved_workspace, model)
    } else {
        let workspace = canonical_workspace(requested_workspace.as_deref().unwrap_or(&launch_dir))?;
        let model = model_override.unwrap_or_else(|| provider_selection.model.clone());
        let session = store.create_session(&workspace, &model)?;
        (session.id, workspace, model)
    };

    workspace_history.record(&workspace);
    initialize_session(&store, &session_id, &workspace, &config, &memory_engine)?;
    let policy = policy_for(cli.sandbox, &workspace);
    provider_selection.model = model.clone();
    let provider = build_provider(&provider_selection, &workspace, policy.mode)?;
    let mut runtime_config = config;
    apply_selection_to_config(&provider_selection, &mut runtime_config);
    let whatsapp_launch = gui::WhatsAppLaunchConfig::from_config(&runtime_config, &app_home);
    let config_state = Arc::new(RwLock::new(runtime_config));
    let models = fetch_model_ids(&config_state, &model).await;
    if provider_selection.provider_id == "openai-account"
        && !models.iter().any(|available| available == &model)
    {
        model = "default".to_string();
        provider_selection.model = model.clone();
        provider_settings.save(&provider_selection)?;
        store.set_model(&session_id, &model)?;
        let mut config = config_state.write().await;
        config.default_model = model.clone();
        config.save(&config_path)?;
    }
    let dream = spawn_dream_worker(
        memory_engine.clone(),
        llama::LlamaClient::new(),
        config_state.clone(),
    );
    let (op_tx, op_rx) = mpsc::channel(256);
    let (event_tx, event_rx) = mpsc::channel(1_024);
    let (approval_tx, approval_rx) = mpsc::channel(64);
    let (privilege_tx, privilege_rx) = mpsc::channel(8);
    let output_store = Arc::new(ToolOutputStore::new(
        app_paths.store_dir.join("tool_outputs"),
    )?);
    let privilege_broker = Arc::new(PrivilegeBroker::new(event_tx.clone(), privilege_rx));

    let mut registry = Registry::default();
    tools::register_all(
        &mut registry,
        &workspace,
        &app_paths.generated_dir,
        policy.clone(),
        config_state.clone(),
        output_store.clone(),
        privilege_broker.clone(),
    );

    let agent = Agent::new(
        provider,
        Arc::new(registry),
        store,
        session_id,
        model,
        approval_for(cli.sandbox),
        96_000,
        workspace,
        policy.clone(),
        output_store,
        event_tx.clone(),
        approval_rx,
    );

    send_ready(&agent, &policy, &config_state, &workspace_history, &models).await;
    if let Some(note) = workspace_note {
        let _ = agent
            .event_sender()
            .send(Event::Notice { message: note })
            .await;
    }
    let core = tokio::spawn(core_loop(
        agent,
        policy,
        provider_selection,
        provider_settings,
        config_state,
        config_path,
        workspace_history,
        app_paths,
        memory_engine,
        dream,
        models,
        op_rx,
        approval_tx,
        privilege_tx,
        privilege_broker,
    ));
    // The native event loop must stay on the process main thread. Tokio's
    // worker threads continue to drive the agent core while eframe blocks
    // here, and the GUI polls the same Op/Event channels the TUI used.
    let ui_result = gui::run(op_tx.clone(), event_rx, whatsapp_launch);
    let _ = op_tx.send(Op::Shutdown).await;
    let core_result = core.await.context("agent core task panicked")?;

    ui_result?;
    core_result
}

/// What the idle op handler asks the loop to do next.
enum IdleOutcome {
    Continue,
    StartTurn(String),
    Shutdown,
}

/// Everything the op handlers need. One struct so the busy branch can defer
/// an op and the drain loop can replay it against identical state.
struct Core {
    agent: Agent,
    policy: SandboxPolicy,
    provider_selection: ProviderSelection,
    provider_settings: ProviderSettingsStore,
    config_state: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    approvals: mpsc::Sender<(String, Decision)>,
    privilege_replies: mpsc::Sender<PrivilegeCredential>,
    privilege_broker: Arc<PrivilegeBroker>,
    workspace_history: WorkspaceHistory,
    app_paths: storage::AppPaths,
    memory: Arc<MemoryEngine>,
    dream: DreamHandle,
    models: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn core_loop(
    agent: Agent,
    policy: SandboxPolicy,
    provider_selection: ProviderSelection,
    provider_settings: ProviderSettingsStore,
    config_state: Arc<RwLock<AppConfig>>,
    config_path: PathBuf,
    workspace_history: WorkspaceHistory,
    app_paths: storage::AppPaths,
    memory: Arc<MemoryEngine>,
    dream: DreamHandle,
    models: Vec<String>,
    mut ops: mpsc::Receiver<Op>,
    approvals: mpsc::Sender<(String, Decision)>,
    privilege_replies: mpsc::Sender<PrivilegeCredential>,
    privilege_broker: Arc<PrivilegeBroker>,
) -> Result<()> {
    let events = agent.event_sender();
    let mut core = Core {
        agent,
        policy,
        provider_selection,
        provider_settings,
        config_state,
        config_path,
        approvals,
        privilege_replies,
        privilege_broker,
        workspace_history,
        app_paths,
        memory,
        dream,
        models,
    };
    let mut active: Option<ActiveTurn> = None;
    let mut queued: VecDeque<String> = VecDeque::new();
    // State-changing ops that arrived mid-turn. Applied in arrival order the
    // moment the turn ends — queued, never dropped.
    let mut deferred: VecDeque<Op> = VecDeque::new();

    loop {
        if let Some(mut turn) = active.take() {
            tokio::select! {
                result = &mut turn.handle => {
                    report_turn_result(&events, result).await;
                    spawn_agent_memory_refresh(&core);
                    // The first user turn assigns the automatic conversation
                    // title; refresh the sidebar as soon as that turn lands.
                    send_session_list(&core, &events).await;
                    let mut shutdown = false;
                    while let Some(op) = deferred.pop_front() {
                        match handle_idle_op(&mut core, op, &events).await? {
                            IdleOutcome::Continue => {}
                            IdleOutcome::StartTurn(text) => queued.push_back(text),
                            IdleOutcome::Shutdown => { shutdown = true; break; }
                        }
                    }
                    if shutdown {
                        break;
                    }
                    if let Some(text) = queued.pop_front() {
                        active = Some(start_turn(&core.agent, text));
                    }
                }
                op = ops.recv() => {
                    let Some(op) = op else {
                        turn.cancel.cancel();
                        let _ = turn.handle.await;
                        break;
                    };
                    match op {
                        Op::Submit { text } => {
                            queued.push_back(text);
                            notice(&events, "message queued").await;
                        }
                        Op::Interrupt => {
                            turn.cancel.cancel();
                        }
                        Op::Approve { call_id, decision } => {
                            let _ = core.approvals.send((call_id, decision)).await;
                        }
                        Op::ProvidePrivilegeCredential {
                            request_id,
                            credential,
                            remember,
                        } => {
                            let _ = core
                                .privilege_replies
                                .send(PrivilegeCredential {
                                    request_id,
                                    credential,
                                    remember,
                                })
                                .await;
                        }
                        Op::Shutdown => {
                            turn.cancel.cancel();
                            let _ = turn.handle.await;
                            break;
                        }
                        // Read-only/history commands make no sense mid-turn.
                        Op::Compact | Op::Rollback | Op::ShowDiff => {
                            notice(&events, "command is available after the current turn").await;
                        }
                        // Everything that changes state is deferred, not lost.
                        other => {
                            deferred.push_back(other);
                            notice(
                                &events,
                                "queued — it will be applied as soon as the current turn ends",
                            )
                            .await;
                        }
                    }
                    active = Some(turn);
                }
            }
            continue;
        }

        let Some(op) = ops.recv().await else {
            break;
        };
        match handle_idle_op(&mut core, op, &events).await? {
            IdleOutcome::Continue => {}
            IdleOutcome::StartTurn(text) => active = Some(start_turn(&core.agent, text)),
            IdleOutcome::Shutdown => break,
        }
    }

    core.dream.shutdown();
    Ok(())
}

async fn handle_idle_op(
    core: &mut Core,
    op: Op,
    events: &mpsc::Sender<Event>,
) -> Result<IdleOutcome> {
    match op {
        Op::Submit { text } => return Ok(IdleOutcome::StartTurn(text)),
        Op::Interrupt => notice(events, "nothing is running").await,
        Op::Approve { call_id, decision } => {
            let _ = core.approvals.send((call_id, decision)).await;
        }
        Op::ProvidePrivilegeCredential {
            request_id,
            credential,
            remember,
        } => {
            let _ = core
                .privilege_replies
                .send(PrivilegeCredential {
                    request_id,
                    credential,
                    remember,
                })
                .await;
        }
        Op::Compact => match core.agent.force_compact().await {
            Ok(true) => {}
            Ok(false) => notice(events, "there is not enough history to compact").await,
            Err(error) => recoverable_error(events, error).await,
        },
        Op::Rollback => {
            if core.policy.mode == SandboxMode::ReadOnly {
                notice(
                    events,
                    "rollback is disabled by the read-only sandbox policy",
                )
                .await;
            } else {
                match core
                    .agent
                    .store
                    .rollback_session(&core.agent.session_id, &core.agent.workspace)
                {
                    Ok(paths) if paths.is_empty() => notice(events, "nothing to roll back").await,
                    Ok(paths) => {
                        notice(
                            events,
                            &format!("rolled back {} file change(s)", paths.len()),
                        )
                        .await
                    }
                    Err(error) => recoverable_error(events, error).await,
                }
            }
        }
        Op::ShowDiff => match core.agent.store.session_diff(&core.agent.session_id) {
            Ok(diff) if diff.trim().is_empty() => notice(events, "no active patch diff").await,
            Ok(diff) => {
                let _ = events
                    .send(Event::PatchApplied {
                        files: Vec::new(),
                        diff,
                    })
                    .await;
            }
            Err(error) => recoverable_error(events, error).await,
        },
        Op::NewSession => {
            let session = core
                .agent
                .store
                .create_session(&core.agent.workspace, &core.agent.model)?;
            let config = core.config_state.read().await.clone();
            initialize_session(
                &core.agent.store,
                &session.id,
                &core.agent.workspace,
                &config,
                &core.memory,
            )?;
            core.agent.switch_session(session.id).await;
            let _ = events.send(Event::SessionReset).await;
            send_ready(
                &core.agent,
                &core.policy,
                &core.config_state,
                &core.workspace_history,
                &core.models,
            )
            .await;
            notice(events, "started a new session").await;
        }
        Op::SetWorkspace { path } => match set_workspace(core, path, events).await {
            Ok(()) => {}
            Err(error) => recoverable_error(events, error).await,
        },
        Op::SetModel { model } => {
            set_model(core, model, events).await?;
            core.models = llama::normalize_model_ids(core.models.clone(), &core.agent.model);
            send_ready(
                &core.agent,
                &core.policy,
                &core.config_state,
                &core.workspace_history,
                &core.models,
            )
            .await;
        }
        Op::SetProvider {
            provider_id,
            api_key,
            base_url,
        } => {
            if let Err(error) = set_provider(
                core,
                provider_id,
                api_key.map(|secret| secret.expose().to_string()),
                base_url,
                events,
            )
            .await
            {
                recoverable_error(events, error).await;
            }
        }
        Op::SetWebSearch { enabled } => {
            set_web_search(core, enabled, events).await?;
        }
        Op::SetSandbox { mode } => {
            set_sandbox(core, &mode, events).await?;
        }
        Op::SetWhatsApp {
            enabled,
            assistant_name,
            has_own_number,
            allowed_jids,
        } => {
            let (assistant_name, allowed_jids) = {
                let mut config = core.config_state.write().await;
                config.whatsapp_enabled = enabled;
                config.whatsapp_assistant_name = assistant_name;
                config.whatsapp_has_own_number = has_own_number;
                config.whatsapp_allowed_jids = allowed_jids;
                config.normalize();
                config.save(&core.config_path)?;
                (
                    config.whatsapp_assistant_name.clone(),
                    config.whatsapp_allowed_jids.clone(),
                )
            };
            let _ = events
                .send(Event::WhatsAppConfigChanged {
                    enabled,
                    assistant_name,
                    has_own_number,
                    allowed_jids,
                })
                .await;
            notice(events, "WhatsApp settings saved").await;
        }
        Op::SetNodeHub {
            enabled,
            bind,
            port,
        } => match bind.trim().parse::<std::net::IpAddr>() {
            Err(_) => {
                recoverable_error(
                    events,
                    anyhow::anyhow!("adresa Hub trebuie să fie un IP valid, de exemplu 0.0.0.0"),
                )
                .await;
            }
            Ok(_) if port == 0 => {
                recoverable_error(events, anyhow::anyhow!("portul Hub nu poate fi 0")).await;
            }
            Ok(_) => {
                let (bind, port) = {
                    let mut config = core.config_state.write().await;
                    config.node_hub_enabled = enabled;
                    config.node_hub_bind = bind;
                    config.node_hub_port = port;
                    config.normalize();
                    config.save(&core.config_path)?;
                    (config.node_hub_bind.clone(), config.node_hub_port)
                };
                let _ = events
                    .send(Event::NodeHubConfigChanged {
                        enabled,
                        bind,
                        port,
                    })
                    .await;
                notice(events, "Node Hub settings saved; restart GnomeAI to apply the listener")
                    .await;
            }
        },
        Op::ListSessions => {
            send_session_list(core, events).await;
        }
        Op::ResumeSession { id } => {
            if let Err(error) = resume_session(core, &id, events).await {
                recoverable_error(events, error).await;
            }
        }
        Op::ForkSession => {
            if let Err(error) = fork_session(core, events).await {
                recoverable_error(events, error).await;
            }
        }
        Op::RenameSession { id, title } => {
            match core.agent.store.rename_session(&id, &title) {
                Ok(()) => notice(events, "session renamed").await,
                Err(error) => recoverable_error(events, error).await,
            }
            send_session_list(core, events).await;
        }
        Op::DeleteSession { id } => {
            if let Err(error) = delete_session(core, &id, events).await {
                recoverable_error(events, error).await;
            }
            send_session_list(core, events).await;
        }
        Op::MemoryShow => {
            show_memory(core, events).await;
        }
        Op::MemoryStatus => {
            let config = core.config_state.read().await.clone();
            match core.memory.status(&config) {
                Ok(status) => notice(events, &status.render_text()).await,
                Err(error) => recoverable_error(events, error).await,
            }
        }
        Op::MemoryClear => match core.memory.clear() {
            Ok(()) => notice(events, "shared memory cleared").await,
            Err(error) => recoverable_error(events, error).await,
        },
        Op::MemoryDream { dry_run } => {
            // Dreaming can run for up to memory_dream_max_seconds; keep the
            // op loop responsive and report when the cycle finishes.
            let dream = core.dream.clone();
            let events = events.clone();
            notice(
                &events,
                if dry_run {
                    "dream dry-run started — the report will follow"
                } else {
                    "dream cycle started — the report will follow"
                },
            )
            .await;
            tokio::spawn(async move {
                match dream.run(dry_run).await {
                    Ok(report) => notice(&events, &report.render_text()).await,
                    Err(error) => recoverable_error(&events, error).await,
                }
            });
        }
        Op::MemoryReindex => {
            let engine = core.memory.clone();
            let config_state = core.config_state.clone();
            let events = events.clone();
            notice(&events, "reindexing memory embeddings…").await;
            tokio::spawn(async move {
                let config = config_state.read().await.clone();
                match engine.reindex(&config).await {
                    Ok(report) => notice(&events, &report.render_text()).await,
                    Err(error) => recoverable_error(&events, error).await,
                }
            });
        }
        Op::MemoryForget { id } => match core.memory.forget(&id) {
            Ok(true) => notice(events, &format!("fact {id} marked as forgotten")).await,
            Ok(false) => notice(events, &format!("no fact with id {id}")).await,
            Err(error) => recoverable_error(events, error).await,
        },
        Op::MemorySet { enabled } => {
            {
                let mut config = core.config_state.write().await;
                config.memory_enabled = enabled;
                config.save(&core.config_path)?;
            }
            notice(
                events,
                if enabled {
                    "shared memory enabled — facts are injected into new sessions and \
                     extracted after each turn"
                } else {
                    "shared memory disabled — nothing is read or written until re-enabled"
                },
            )
            .await;
        }
        Op::SkillsList => {
            notice(events, &skills::render_catalog(&core.agent.workspace)).await;
        }
        Op::SkillInspect { name } => match skills::inspect(&core.agent.workspace, &name) {
            Ok(report) => notice(events, &report).await,
            Err(error) => recoverable_error(events, error).await,
        },
        Op::SkillActivate { name } => match skills::load(&core.agent.workspace, &name) {
            Ok(skill) => {
                let block = skills::render_for_model(&skill);
                core.agent.store.append_turn(
                    &core.agent.session_id,
                    "system",
                    &block,
                    (block.len() / 4) as i64,
                    true,
                )?;
                notice(
                    events,
                    &format!(
                        "skill `{}` activated for this session; its requested tools do not \
                         bypass the current execution mode",
                        skill.summary.name
                    ),
                )
                .await;
            }
            Err(error) => recoverable_error(events, error).await,
        },
        Op::SkillInstall { source } => {
            notice(events, "installing and validating skill…").await;
            let workspace = core.agent.workspace.clone();
            match tokio::task::spawn_blocking(move || skills::install(&source, &workspace)).await {
                Ok(Ok(skill)) => {
                    append_skill_catalog_update(core)?;
                    notice(
                        events,
                        &format!(
                            "installed `{}` from a validated SKILL.md package",
                            skill.name
                        ),
                    )
                    .await;
                }
                Ok(Err(error)) => recoverable_error(events, error).await,
                Err(error) => recoverable_error(events, error).await,
            }
        }
        Op::SkillUpdate { name } => {
            notice(events, "updating and re-validating skill…").await;
            let workspace = core.agent.workspace.clone();
            match tokio::task::spawn_blocking(move || skills::update(&name, &workspace)).await {
                Ok(Ok(skill)) => {
                    append_skill_catalog_update(core)?;
                    notice(events, &format!("updated `{}`", skill.name)).await;
                }
                Ok(Err(error)) => recoverable_error(events, error).await,
                Err(error) => recoverable_error(events, error).await,
            }
        }
        Op::SkillVerify { name } => match skills::verify(&core.agent.workspace, &name) {
            Ok(report) => notice(events, &report).await,
            Err(error) => recoverable_error(events, error).await,
        },
        Op::SkillRemove { name } => match skills::remove(&name) {
            Ok(()) => {
                append_skill_catalog_update(core)?;
                notice(events, &format!("removed managed skill `{name}`")).await;
            }
            Err(error) => recoverable_error(events, error).await,
        },
        Op::Doctor => {
            let report = run_doctor(core).await;
            notice(events, &report).await;
        }
        Op::Shutdown => return Ok(IdleOutcome::Shutdown),
    }
    Ok(IdleOutcome::Continue)
}

fn append_skill_catalog_update(core: &Core) -> Result<()> {
    let catalog = skills::catalog_prompt(&core.agent.workspace);
    let content = if catalog.is_empty() {
        "[Agent Skills catalog update]\nNo Agent Skills are currently installed.".to_string()
    } else {
        format!("[Agent Skills catalog update]{catalog}")
    };
    core.agent.store.append_turn(
        &core.agent.session_id,
        "system",
        &content,
        (content.len() / 4) as i64,
        true,
    )?;
    Ok(())
}

fn start_turn(agent: &Agent, text: String) -> ActiveTurn {
    let agent = agent.clone();
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();
    let handle = tokio::spawn(async move { agent.run_turn(text, turn_cancel).await });
    ActiveTurn { cancel, handle }
}

async fn report_turn_result(
    events: &mpsc::Sender<Event>,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => recoverable_error(events, error).await,
        Err(error) => {
            let _ = events
                .send(Event::Error {
                    message: format!("agent turn task failed: {error}"),
                    fatal: false,
                })
                .await;
        }
    }
}

async fn set_model(core: &mut Core, model: String, events: &mpsc::Sender<Event>) -> Result<()> {
    let model = model.trim();
    if model.is_empty() {
        notice(events, "model name cannot be empty").await;
        return Ok(());
    }
    if core.provider_selection.provider_id == "openai-account"
        && !core.models.iter().any(|available| available == model)
    {
        notice(
            events,
            "modelul nu este disponibil pentru contul OpenAI conectat; reîncarcă providerul și alege un model din listă",
        )
        .await;
        return Ok(());
    }
    core.agent.model = model.to_string();
    core.agent.store.set_model(&core.agent.session_id, model)?;
    core.provider_selection.model = model.to_string();
    core.provider_settings.save(&core.provider_selection)?;
    {
        let mut config = core.config_state.write().await;
        config.default_model = model.to_string();
        config.save(&core.config_path)?;
    }
    notice(events, &format!("model set to {model}")).await;
    Ok(())
}

async fn set_provider(
    core: &mut Core,
    provider_id: String,
    api_key: Option<String>,
    base_url: Option<String>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let api_key = core
        .config_state
        .read()
        .await
        .resolve_provider_api_key(&provider_id, api_key);
    let selection = ProviderSelection::from_choice(provider_id, api_key, base_url)?;
    let provider = build_provider(&selection, &core.agent.workspace, core.policy.mode)?;
    core.provider_settings.save(&selection)?;
    core.agent
        .store
        .set_model(&core.agent.session_id, &selection.model)?;
    {
        let mut config = core.config_state.write().await;
        apply_selection_to_config(&selection, &mut config);
        config.save(&core.config_path)?;
    }

    core.agent.provider = provider;
    core.agent.model = selection.model.clone();
    core.provider_selection = selection;

    // Fetch once, outside the configuration lock. The helper falls back to
    // the maintained provider catalog and always keeps the active model.
    core.models = fetch_model_ids(&core.config_state, &core.agent.model).await;

    let _ = events
        .send(Event::ProviderChanged {
            provider: core.agent.provider.name().to_string(),
            model: core.agent.model.clone(),
            models: core.models.clone(),
        })
        .await;
    send_ready(
        &core.agent,
        &core.policy,
        &core.config_state,
        &core.workspace_history,
        &core.models,
    )
    .await;

    if matches!(
        preset(&core.provider_selection.provider_id).map(|provider| provider.protocol),
        Some(provider_catalog::WireProtocol::CodexAppServer)
            | Some(provider_catalog::WireProtocol::ClaudeCli)
    ) {
        notice(
            events,
            "account mode delegates each turn to the vendor runtime and uses its saved login",
        )
        .await;
    } else {
        notice(
            events,
            &format!("provider set to {}", core.agent.provider.name()),
        )
        .await;
    }
    Ok(())
}

async fn set_sandbox(core: &mut Core, mode: &str, events: &mpsc::Sender<Event>) -> Result<()> {
    let mode = match parse_sandbox(mode) {
        Ok(mode) => mode,
        Err(error) => {
            recoverable_error(events, error).await;
            return Ok(());
        }
    };
    let policy = policy_for(mode, &core.agent.workspace);
    let mut registry = Registry::default();
    tools::register_all(
        &mut registry,
        &core.agent.workspace,
        &core.app_paths.generated_dir,
        policy.clone(),
        core.config_state.clone(),
        core.agent.output_store.clone(),
        core.privilege_broker.clone(),
    );
    core.agent.registry = Arc::new(registry);
    core.agent.verify_policy = policy.clone();
    core.agent.approval = approval_for(mode);
    // Account-backed CLIs receive the current sandbox mode as command-line
    // policy, so rebuild the adapter whenever it changes.
    core.agent.provider =
        build_provider(&core.provider_selection, &core.agent.workspace, policy.mode)?;
    core.policy = policy;
    {
        let mut config = core.config_state.write().await;
        config.web_sandbox_mode = sandbox_name(mode).to_string();
        config.save(&core.config_path)?;
    }
    send_ready(
        &core.agent,
        &core.policy,
        &core.config_state,
        &core.workspace_history,
        &core.models,
    )
    .await;
    notice(
        events,
        match mode {
            SandboxMode::ReadOnly => {
                "read-only enabled — commands stay isolated and cannot modify the workspace"
            }
            SandboxMode::Normal => {
                "normal enabled — commands have normal OS access only after your approval"
            }
            SandboxMode::FullAccess => {
                "WARNING: full-access enabled — commands run with normal OS access and no approvals"
            }
            SandboxMode::IsolatedWorkspaceWrite => {
                "internal isolated-workspace-write policy enabled"
            }
        },
    )
    .await;
    Ok(())
}

async fn set_workspace(
    core: &mut Core,
    requested: PathBuf,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let workspace = resolve_workspace_request(&core.agent.workspace, &requested)?;
    if workspace == core.agent.workspace {
        notice(
            events,
            &format!("workspace is already {}", workspace.display()),
        )
        .await;
        return Ok(());
    }

    // Build every path-bound component before mutating the live agent. A
    // provider/configuration error therefore leaves the current workspace
    // fully usable.
    let policy = policy_for(core.policy.mode, &workspace);
    let provider = build_provider(&core.provider_selection, &workspace, policy.mode)?;
    let mut registry = Registry::default();
    tools::register_all(
        &mut registry,
        &workspace,
        &core.app_paths.generated_dir,
        policy.clone(),
        core.config_state.clone(),
        core.agent.output_store.clone(),
        core.privilege_broker.clone(),
    );
    let session = core
        .agent
        .store
        .create_session(&workspace, &core.agent.model)?;
    let config = core.config_state.read().await.clone();
    initialize_session(
        &core.agent.store,
        &session.id,
        &workspace,
        &config,
        &core.memory,
    )?;

    core.agent.workspace = workspace.clone();
    core.agent.verify_policy = policy.clone();
    core.agent.registry = Arc::new(registry);
    core.agent.provider = provider;
    core.agent.switch_session(session.id).await;
    core.policy = policy;
    core.workspace_history.record(&workspace);

    let _ = events.send(Event::SessionReset).await;
    send_ready(
        &core.agent,
        &core.policy,
        &core.config_state,
        &core.workspace_history,
        &core.models,
    )
    .await;
    notice(
        events,
        &format!("workspace changed to {}", workspace.display()),
    )
    .await;
    Ok(())
}

async fn set_web_search(core: &Core, enabled: bool, events: &mpsc::Sender<Event>) -> Result<()> {
    {
        let mut config = core.config_state.write().await;
        config.web_search_enabled = enabled;
        config.save(&core.config_path)?;
    }
    let _ = events.send(Event::WebSearchChanged { enabled }).await;
    notice(
        events,
        if enabled {
            "web search enabled; local Firecrawl starts on the first web request"
        } else {
            "web search disabled"
        },
    )
    .await;
    send_ready(
        &core.agent,
        &core.policy,
        &core.config_state,
        &core.workspace_history,
        &core.models,
    )
    .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------

async fn send_session_list(core: &Core, events: &mpsc::Sender<Event>) {
    let sessions = match core.agent.store.recent_sessions(20) {
        Ok(sessions) => sessions,
        Err(error) => {
            recoverable_error(events, error).await;
            return;
        }
    };
    let summaries = sessions
        .into_iter()
        .map(|session| {
            // Backfill older untitled sessions from their first real user
            // message so the sidebar is useful immediately after upgrading.
            let title = session.title.or_else(|| {
                let first = core
                    .agent
                    .store
                    .live_turns(&session.id)
                    .ok()?
                    .into_iter()
                    .find(|turn| turn.role == "user" && !turn.is_summary)?;
                let visible = provider::user_content_for_display(&first.content);
                let title = agent::automatic_session_title(&visible);
                core.agent.store.rename_session(&session.id, &title).ok()?;
                Some(title)
            });
            SessionSummary {
                turns: core.agent.store.count_turns(&session.id).unwrap_or(0),
                is_current: session.id == core.agent.session_id,
                id: session.id,
                title,
                workspace: session.workspace,
                model: session.model,
                updated_at: session.updated_at,
            }
        })
        .collect();
    let _ = events
        .send(Event::SessionList {
            sessions: summaries,
        })
        .await;
}

async fn resume_session(core: &mut Core, id: &str, events: &mpsc::Sender<Event>) -> Result<()> {
    let session = core
        .agent
        .store
        .get_session(id)?
        .with_context(|| format!("session `{id}` does not exist"))?;
    let workspace = canonical_workspace(&session.workspace)?;

    if workspace != core.agent.workspace {
        let policy = policy_for(core.policy.mode, &workspace);
        let provider = build_provider(&core.provider_selection, &workspace, policy.mode)?;
        let mut registry = Registry::default();
        tools::register_all(
            &mut registry,
            &workspace,
            &core.app_paths.generated_dir,
            policy.clone(),
            core.config_state.clone(),
            core.agent.output_store.clone(),
            core.privilege_broker.clone(),
        );
        core.agent.workspace = workspace.clone();
        core.agent.verify_policy = policy.clone();
        core.agent.registry = Arc::new(registry);
        core.agent.provider = provider;
        core.policy = policy;
        core.workspace_history.record(&workspace);
    }

    core.agent.model = session.model.clone();
    core.agent.switch_session(session.id.clone()).await;

    let _ = events.send(Event::SessionReset).await;
    let turns = session_history_turns(&core.agent)?;
    if !turns.is_empty() {
        let _ = events.send(Event::HistoryReplay { turns }).await;
    }
    send_ready(
        &core.agent,
        &core.policy,
        &core.config_state,
        &core.workspace_history,
        &core.models,
    )
    .await;
    notice(
        events,
        &format!(
            "resumed session {} in {}",
            &session.id[..8.min(session.id.len())],
            workspace.display()
        ),
    )
    .await;
    Ok(())
}

async fn fork_session(core: &mut Core, events: &mpsc::Sender<Event>) -> Result<()> {
    let tip = core.agent.store.latest_seq(&core.agent.session_id)?;
    let fork = core.agent.store.fork(&core.agent.session_id, tip)?;
    core.agent.switch_session(fork.id.clone()).await;

    let _ = events.send(Event::SessionReset).await;
    let turns = session_history_turns(&core.agent)?;
    if !turns.is_empty() {
        let _ = events.send(Event::HistoryReplay { turns }).await;
    }
    send_ready(
        &core.agent,
        &core.policy,
        &core.config_state,
        &core.workspace_history,
        &core.models,
    )
    .await;
    notice(
        events,
        &format!(
            "forked into session {} — the original is untouched",
            &fork.id[..8.min(fork.id.len())]
        ),
    )
    .await;
    Ok(())
}

async fn delete_session(core: &mut Core, id: &str, events: &mpsc::Sender<Event>) -> Result<()> {
    let deleting_current = id == core.agent.session_id;
    core.agent.store.delete_session(id)?;
    if deleting_current {
        let session = core
            .agent
            .store
            .create_session(&core.agent.workspace, &core.agent.model)?;
        let config = core.config_state.read().await.clone();
        initialize_session(
            &core.agent.store,
            &session.id,
            &core.agent.workspace,
            &config,
            &core.memory,
        )?;
        core.agent.switch_session(session.id).await;
        let _ = events.send(Event::SessionReset).await;
        send_ready(
            &core.agent,
            &core.policy,
            &core.config_state,
            &core.workspace_history,
            &core.models,
        )
        .await;
        notice(events, "deleted the active session and started a new one").await;
    } else {
        notice(events, "session deleted").await;
    }
    Ok(())
}

/// The user-visible turns of the active session, oldest first.
fn session_history_turns(agent: &Agent) -> Result<Vec<HistoryTurn>> {
    let mut out = Vec::new();
    for turn in agent.store.live_turns(&agent.session_id)? {
        match turn.role.as_str() {
            _ if turn.is_summary => out.push(HistoryTurn {
                role: "note".into(),
                text: format!("[compacted context]\n{}", turn.content),
            }),
            "user" => {
                if turn.content.starts_with("[automatic verification]") {
                    continue;
                }
                out.push(HistoryTurn {
                    role: "user".into(),
                    text: provider::user_content_for_display(&turn.content),
                });
            }
            "assistant" => {
                if let Ok(provider::Message::Assistant { content, .. }) =
                    serde_json::from_str(&turn.content)
                    && !content.trim().is_empty()
                {
                    out.push(HistoryTurn {
                        role: "assistant".into(),
                        text: content,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Shared cross-conversation memory
// ---------------------------------------------------------------------------

async fn show_memory(core: &Core, events: &mpsc::Sender<Event>) {
    let config = core.config_state.read().await.clone();
    let facts = match core.memory.list_facts(50, false) {
        Ok(facts) => facts,
        Err(error) => {
            recoverable_error(events, error).await;
            return;
        }
    };
    let active_count = core
        .memory
        .status(&config)
        .map(|status| status.active_facts)
        .unwrap_or(facts.len() as i64);
    let mut lines = vec![format!(
        "shared memory ({}) — {} active fact(s), age filter: {}",
        if config.memory_enabled {
            "enabled"
        } else {
            "disabled"
        },
        active_count,
        if config.memory_max_age_days == 0 {
            "none".to_string()
        } else {
            format!("{} days", config.memory_max_age_days)
        },
    )];
    for fact in &facts {
        lines.push(format!(
            "  [{} | {} | {:.2} | {}] {}",
            fact.id,
            fact.category.as_str(),
            fact.confidence,
            fact.source_channel,
            fact.text
        ));
    }
    if active_count > facts.len() as i64 {
        lines.push(format!(
            "  … and {} more",
            active_count - facts.len() as i64
        ));
    }
    lines.push(
        "commands: /memory status · dream [--dry-run] · reindex · forget ID · clear · on · off"
            .into(),
    );
    notice(events, &lines.join("\n")).await;
}

/// After every finished turn, extract durable facts into the shared SQLite
/// store — the same `store/memory.db` WebTool and WhatsApp use.
fn spawn_agent_memory_refresh(core: &Core) {
    let agent = core.agent.clone();
    let config_state = core.config_state.clone();
    let engine = core.memory.clone();
    tokio::spawn(async move {
        let config = config_state.read().await.clone();
        if !config.memory_enabled {
            return;
        }
        // Account-backed CLI providers have no reusable HTTP endpoint here.
        if !matches!(config.provider_protocol.as_str(), "openai" | "anthropic") {
            return;
        }
        let chat = match agent_session_as_chat(&agent) {
            Ok(Some(chat)) => chat,
            _ => return,
        };
        let client = llama::LlamaClient::new();
        if let Err(error) = engine
            .extract_from_chat(&client, &config, &agent.model, &chat, "tui")
            .await
        {
            tracing::warn!("agent memory refresh failed: {error}");
        }
    });
}

/// Project the SQLite transcript into the WebTool `Chat` shape the memory
/// extractor understands. Tool output stays out: it is workspace detail, not
/// durable knowledge about the user.
fn agent_session_as_chat(agent: &Agent) -> Result<Option<storage::Chat>> {
    let mut messages = Vec::new();
    for turn in agent.store.live_turns(&agent.session_id)? {
        match turn.role.as_str() {
            "user" if !turn.is_summary && !turn.content.starts_with("[automatic verification]") => {
                messages.push(agent_chat_message("user", &turn.content));
            }
            "assistant" => {
                if let Ok(provider::Message::Assistant { content, .. }) =
                    serde_json::from_str(&turn.content)
                    && !content.trim().is_empty()
                {
                    messages.push(agent_chat_message("assistant", &content));
                }
            }
            _ => {}
        }
    }
    if messages.len() < 2 {
        return Ok(None);
    }
    Ok(Some(storage::Chat {
        id: format!("agent_{}", agent.session_id),
        title: format!("Agent session in {}", agent.workspace.display()),
        created: chrono::Utc::now(),
        messages,
        extra: serde_json::Map::new(),
    }))
}

fn agent_chat_message(role: &str, text: &str) -> storage::ChatMessage {
    storage::ChatMessage {
        role: role.to_string(),
        content: serde_json::Value::String(text.to_string()),
        timestamp: chrono::Utc::now(),
        extra: serde_json::Map::new(),
    }
}

// ---------------------------------------------------------------------------
// /doctor
// ---------------------------------------------------------------------------

async fn run_doctor(core: &Core) -> String {
    let config = core.config_state.read().await.clone();
    let mut lines = vec![format!(
        "doctor report — gnomef-rs v{}",
        env!("CARGO_PKG_VERSION")
    )];
    fn check(lines: &mut Vec<String>, ok: bool, label: String) {
        lines.push(format!("{} {label}", if ok { "✓" } else { "✗" }));
    }

    // State directory and permissions.
    let app_dir = &core.app_paths.app_dir;
    let probe = app_dir.join(".doctor-probe");
    let writable = std::fs::write(&probe, b"ok").is_ok();
    std::fs::remove_file(&probe).ok();
    check(
        &mut lines,
        writable,
        format!("state directory writable: {}", app_dir.display()),
    );
    check(
        &mut lines,
        file_is_private(&core.config_path),
        format!("config permissions 0600: {}", core.config_path.display()),
    );
    let providers_file = core.app_paths.store_dir.join("providers.json");
    check(
        &mut lines,
        file_is_private(&providers_file),
        format!(
            "provider store permissions 0600: {}",
            providers_file.display()
        ),
    );

    // Database.
    match core.agent.store.health() {
        Ok(verdict) if verdict == "ok" => {
            check(&mut lines, true, "agent database integrity: ok".into())
        }
        Ok(verdict) => check(
            &mut lines,
            false,
            format!("agent database integrity: {verdict}"),
        ),
        Err(error) => check(
            &mut lines,
            false,
            format!("agent database check failed: {error}"),
        ),
    }

    // Workspace.
    let workspace = &core.agent.workspace;
    check(
        &mut lines,
        workspace.is_dir(),
        format!("workspace exists: {}", workspace.display()),
    );
    let ws_probe = workspace.join(".gnomef-doctor-probe");
    let ws_writable = std::fs::write(&ws_probe, b"ok").is_ok();
    std::fs::remove_file(&ws_probe).ok();
    check(&mut lines, ws_writable, "workspace writable".into());
    check(
        &mut lines,
        workspace.join(".git").exists(),
        "workspace is a git repository (rollback safety net)".into(),
    );

    // Optional privilege support. sudo is required for the native root tool;
    // secret-tool only adds encrypted desktop-keyring persistence.
    check(
        &mut lines,
        firecrawl::command_in_path("sudo"),
        "sudo executable available for privileged tools".into(),
    );
    lines.push(if firecrawl::command_in_path("secret-tool") {
        "✓ desktop keyring available for optional sudo credential storage".into()
    } else {
        "· desktop keyring helper absent — sudo passwords remain session-only".into()
    });

    // Provider.
    let selection = &core.provider_selection;
    lines.push(format!(
        "· provider: {} · model: {} · protocol: {}",
        selection.provider_id,
        selection.model,
        selection.protocol_name()
    ));
    match selection.protocol_name() {
        "openai" | "anthropic" => {
            let base = selection
                .resolved_base_url()
                .unwrap_or(&config.llama_base_url)
                .to_string();
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(8))
                .build();
            match client {
                Ok(client) => {
                    let url = format!("{}/models", base.trim_end_matches('/'));
                    let mut request = client.get(&url);
                    if selection.protocol_name() == "anthropic" {
                        request = request
                            .header("x-api-key", selection.api_key().unwrap_or_default())
                            .header("anthropic-version", "2023-06-01");
                    } else if let Some(key) = selection.api_key() {
                        request = request.bearer_auth(key);
                    }
                    match request.send().await {
                        Ok(response) => {
                            let status = response.status();
                            check(
                                &mut lines,
                                status.is_success(),
                                format!("provider endpoint {url} answered {status}"),
                            );
                        }
                        Err(error) => check(
                            &mut lines,
                            false,
                            format!("provider endpoint {url} unreachable: {error}"),
                        ),
                    }
                }
                Err(error) => check(
                    &mut lines,
                    false,
                    format!("cannot build HTTP client: {error}"),
                ),
            }
        }
        "codex" => check(
            &mut lines,
            codex_app_server::codex_executable().exists() || firecrawl::command_in_path("codex"),
            "codex sidecar executable present".into(),
        ),
        "claude-cli" => check(
            &mut lines,
            firecrawl::command_in_path("claude"),
            "`claude` CLI present in PATH".into(),
        ),
        other => lines.push(format!(
            "· unknown protocol `{other}` — skipped connectivity"
        )),
    }

    // Web search / Firecrawl.
    if config.web_search_enabled {
        check(&mut lines, true, "web search: enabled".into());
        let local = config.firecrawl_api_url.starts_with("http://127.0.0.1")
            || config.firecrawl_api_url.contains("localhost");
        if local {
            check(
                &mut lines,
                firecrawl::command_in_path("podman"),
                "podman available for the local Firecrawl deployment".into(),
            );
        }
        let reachable = reqwest::Client::new()
            .get(&config.firecrawl_api_url)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .is_ok();
        check(
            &mut lines,
            reachable,
            format!(
                "Firecrawl reachable at {} {}",
                config.firecrawl_api_url,
                if reachable {
                    ""
                } else {
                    "(it starts lazily on the first search)"
                }
            ),
        );
    } else {
        lines.push("· web search: disabled — Firecrawl will not be started".into());
    }

    // Shared memory.
    match core.memory.status(&config) {
        Ok(status) => check(
            &mut lines,
            true,
            format!(
                "shared memory readable ({} active facts, enabled: {}, embeddings: {})",
                status.active_facts,
                config.memory_enabled,
                status
                    .embedding_provider
                    .as_deref()
                    .unwrap_or("lexical only")
            ),
        ),
        Err(error) => check(
            &mut lines,
            false,
            format!("shared memory unreadable: {error}"),
        ),
    }

    // Sandbox support.
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string();
    lines.push(format!(
        "· kernel {kernel} — Landlock needs ≥ 5.13; sandbox mode: {}",
        sandbox_name(core.policy.mode)
    ));

    lines.join("\n")
}

fn file_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.permissions().mode() & 0o077 == 0,
        // A missing file leaks nothing.
        Err(_) => true,
    }
}

fn apply_selection_to_config(selection: &ProviderSelection, config: &mut AppConfig) {
    config.provider_id = selection.provider_id.clone();
    config.provider_protocol = selection.protocol_name().to_string();
    config.default_model = selection.model.clone();
    config.remember_provider_api_key(&selection.provider_id, selection.api_key());
    config.llama_api_key = selection.api_key().unwrap_or_default().to_string();
    if let Some(base_url) = selection.resolved_base_url() {
        config.llama_base_url = base_url.to_string();
    }
}

async fn send_ready(
    agent: &Agent,
    policy: &SandboxPolicy,
    config_state: &Arc<RwLock<AppConfig>>,
    workspace_history: &WorkspaceHistory,
    models: &[String],
) {
    let config = config_state.read().await;
    let web_search_enabled = config.web_search_enabled;
    let models = if config.provider_id == "openai-account" {
        let metadata = models
            .iter()
            .map(|id| llama::ModelInfo {
                id: id.clone(),
                capabilities: Vec::new(),
            })
            .collect();
        llama::codex_account_model_ids(metadata, &agent.model)
    } else {
        llama::normalize_model_ids(models.to_vec(), &agent.model)
    };
    drop(config);
    let _ = agent
        .event_sender()
        .send(Event::Ready {
            session_id: agent.session_id.clone(),
            provider: agent.provider.name().to_string(),
            model: agent.model.clone(),
            workspace: agent.workspace.clone(),
            sandbox: sandbox_name(policy.mode).to_string(),
            web_search_enabled,
            git_branch: git_branch(&agent.workspace),
            recent_workspaces: workspace_history
                .recent()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            models,
        })
        .await;
}

async fn fetch_model_ids(config_state: &Arc<RwLock<AppConfig>>, active_model: &str) -> Vec<String> {
    let cfg = config_state.read().await.clone();
    let models = llama::LlamaClient::new()
        .list_models(&cfg)
        .await
        .unwrap_or_else(|_| llama::known_models(&cfg.provider_id));
    if cfg.provider_id == "openai-account" {
        llama::codex_account_model_ids(models, active_model)
    } else {
        llama::model_ids(models, active_model)
    }
}

fn initialize_session(
    store: &Store,
    session_id: &str,
    workspace: &Path,
    config: &AppConfig,
    engine: &MemoryEngine,
) -> Result<()> {
    if store.live_turns(session_id)?.is_empty() {
        let mut prompt = tools::build_system_prompt(workspace);
        // Shared cross-conversation memory, same database WebTool maintains.
        if config.memory_enabled {
            let block = engine.agent_memory_block(config);
            if !block.is_empty() {
                prompt = memory::append_memory_block(&prompt, Some(&block));
            }
        }
        store.append_turn(
            session_id,
            "system",
            &prompt,
            (prompt.len() / 4) as i64,
            true,
        )?;
    }
    Ok(())
}

fn policy_for(mode: SandboxMode, workspace: &Path) -> SandboxPolicy {
    match mode {
        SandboxMode::ReadOnly => SandboxPolicy::read_only(workspace),
        SandboxMode::Normal => SandboxPolicy::normal(workspace),
        SandboxMode::FullAccess => SandboxPolicy::full_access(workspace),
        SandboxMode::IsolatedWorkspaceWrite => SandboxPolicy::isolated_workspace_write(workspace),
    }
}

fn approval_for(mode: SandboxMode) -> ApprovalPolicy {
    if mode == SandboxMode::FullAccess {
        ApprovalPolicy::Never
    } else {
        ApprovalPolicy::Ask
    }
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        bail!("workspace is not a directory: {}", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("cannot resolve workspace {}", path.display()))
}

fn resolve_workspace_request(current: &Path, requested: &Path) -> Result<PathBuf> {
    let display = requested.to_string_lossy();
    let expanded = if display == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("cannot expand `~`: HOME is not set")?
    } else if let Some(rest) = display.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("cannot expand `~`: HOME is not set")?
            .join(rest)
    } else if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        current.join(requested)
    };
    canonical_workspace(&expanded)
}

fn git_branch(workspace: &Path) -> Option<String> {
    let head = std::fs::read_to_string(workspace.join(".git/HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.trim().chars().take(12).collect()))
}

async fn notice(events: &mpsc::Sender<Event>, message: &str) {
    let _ = events
        .send(Event::Notice {
            message: message.to_string(),
        })
        .await;
}

async fn recoverable_error(events: &mpsc::Sender<Event>, error: impl std::fmt::Display) {
    let _ = events
        .send(Event::Error {
            message: error.to_string(),
            fatal: false,
        })
        .await;
}

fn parse_cli() -> Result<Cli> {
    let mut cli = Cli {
        workspace: None,
        session: None,
        model: None,
        base_url: None,
        sandbox: SandboxMode::Normal,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-w" | "--workspace" => {
                cli.workspace = Some(PathBuf::from(next_value(&mut args, &arg)?))
            }
            "--session" => cli.session = Some(next_value(&mut args, &arg)?),
            "-m" | "--model" => cli.model = Some(next_value(&mut args, &arg)?),
            "--base-url" => cli.base_url = Some(next_value(&mut args, &arg)?),
            "--approval" => {
                let legacy = next_value(&mut args, &arg)?;
                cli.sandbox = match legacy.as_str() {
                    "never" => SandboxMode::FullAccess,
                    "untrusted" | "on-failure" | "on-request" => SandboxMode::Normal,
                    _ => bail!(
                        "legacy approval must be untrusted|on-failure|on-request|never; \
                         prefer --sandbox normal|full-access"
                    ),
                };
            }
            "--sandbox" => {
                cli.sandbox = parse_sandbox(&next_value(&mut args, &arg)?)?;
            }
            value if value.starts_with('-') => bail!("unknown option `{value}`"),
            value if cli.workspace.is_none() => cli.workspace = Some(PathBuf::from(value)),
            value => bail!("unexpected argument `{value}`"),
        }
    }

    Ok(cli)
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("option `{option}` requires a value"))
}

fn parse_sandbox(value: &str) -> Result<SandboxMode> {
    match value {
        "read-only" => Ok(SandboxMode::ReadOnly),
        "normal" | "workspace-write" => Ok(SandboxMode::Normal),
        "full-access" | "danger-full-access" => Ok(SandboxMode::FullAccess),
        _ => bail!("mode must be read-only|normal|full-access"),
    }
}

fn sandbox_name(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::Normal => "normal",
        SandboxMode::FullAccess => "full-access",
        SandboxMode::IsolatedWorkspaceWrite => "isolated-workspace-write",
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn print_help() {
    println!(
        "gnomef-rs [WORKSPACE] [OPTIONS]\n\
         \n\
         Options:\n\
           -w, --workspace PATH       Repository to work in\n\
               --session ID          Resume an existing agent session\n\
           -m, --model MODEL          Override the configured model\n\
               --base-url URL        Override the OpenAI-compatible base URL\n\
               --sandbox MODE        read-only|normal|full-access\n\
           -h, --help                 Show this help\n\
         \n\
         Environment overrides: GNOMEF_MODEL, GNOMEF_BASE_URL, GNOMEF_API_KEY,\n\
         GNOMEF_CODEX_BIN, and GNOMEF_RS_HOME."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_request_resolves_relative_directory_from_current_workspace() {
        let root = std::env::temp_dir().join(format!(
            "gnomef-workspace-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let child = root.join("project");
        std::fs::create_dir_all(&child).unwrap();

        let resolved = resolve_workspace_request(&root, Path::new("project")).unwrap();
        assert_eq!(resolved, child.canonicalize().unwrap());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_request_rejects_missing_directory() {
        let root = std::env::temp_dir().join(format!(
            "gnomef-workspace-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let error = resolve_workspace_request(&root, Path::new("missing"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("workspace is not a directory"));

        std::fs::remove_dir_all(root).unwrap();
    }
}

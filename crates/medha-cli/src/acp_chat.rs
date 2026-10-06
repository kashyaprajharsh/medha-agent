//! One chat over the editor bridge, from start to finish: built, run on a byte
//! stream in each direction, and shut down. A process serving one chat hands it
//! its own stdin and stdout; a backend hands each chat a channel of its own.

use std::sync::Arc;

use runtime::session::{Start, Started};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{acp, acp_questions, desktop_extensions, plugin_session};

pub(crate) async fn run<R, W>(
    start: Start<'_>,
    input: R,
    output: W,
    restore: Option<Value>,
    control: Option<Arc<acp::TurnControl>>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (lock, notices) = (start.lock, start.notices);
    let search_env = start.options.search_env.clone();
    let model_env = start.options.model_env.clone();
    let flag_model = start.options.model.clone();
    let flag_base_url = start.options.base_url.clone();
    let mut bridge = None;
    let Started {
        kernel,
        session,
        system,
        resumed,
        model_name,
        model_profiles,
        active_profile,
        base_budget,
        agent_budget,
        agent_control,
        workspace,
        skill_store,
        memory_store,
        k3_budget_tokens,
        stale_after_days,
        ui_config,
        search_handle,
        lsp_manager,
        mcp_manager,
        session_plugins,
        configured_mcp,
        plugin_mcp_ids,
        scratch: _scratch,
        ..
    } = runtime::session::start(start, |cwd, _| {
        let mut made = acp::bridge_to(output, cwd.to_path_buf());
        if let Some(control) = control {
            made.writer.enable_presentation();
            made.control = control;
        }
        let surface = runtime::Surface {
            gate: Arc::new(acp::AcpGate::new(
                made.writer.clone(),
                made.pending.clone(),
                made.peer.clone(),
            )),
            asker: Arc::new(acp_questions::AcpAsker {
                writer: Arc::clone(&made.writer),
                pending: Arc::clone(&made.questions),
                peer: made.peer.clone(),
                next_id: std::sync::atomic::AtomicU64::new(1),
            }),
            agents: Some(Arc::new(crate::acp_agents::Watch {
                writer: Arc::clone(&made.writer),
                peer: made.peer.clone(),
            })),
        };
        bridge = Some(made);
        surface
    })
    .await?;
    let bridge = bridge.expect("a started chat has chosen its surface");
    tracing::info!(model = %model_name, mode = "acp", "medha session start");

    let surface_result = acp::run(
        kernel.clone(),
        session,
        system,
        model_name,
        base_budget.clone(),
        Arc::clone(&agent_budget),
        resumed,
        bridge,
        agent_control.clone(),
        model_profiles,
        active_profile,
        desktop_extensions::Runtime {
            store: session_plugins.store(),
            skills: skill_store.clone(),
            search: search_handle.clone(),
            search_env,
            model_env,
            flag_model,
            flag_base_url,
            ui: ui_config,
            workspace: workspace.clone(),
            memory: lock.memory.enabled.then(|| memory_store.clone()),
            memory_budget: k3_budget_tokens,
            memory_stale_days: stale_after_days,
            mcp: mcp_manager.clone(),
            configured_mcp: std::sync::Mutex::new(configured_mcp.clone()),
            plugins: plugin_session::LivePlugins::new(
                session_plugins.store(),
                skill_store.clone(),
                mcp_manager.clone(),
                Box::new({
                    let kernel = kernel.clone();
                    move || kernel.reload_hooks()
                }),
                configured_mcp.clone(),
                plugin_mcp_ids,
            ),
        },
        input,
        restore,
        notices,
    )
    .await;
    if let Some(control) = &agent_control {
        control.shutdown().await;
    }
    if let Some(manager) = &lsp_manager {
        manager.shutdown_all().await;
    }
    if let Some(manager) = &mcp_manager {
        manager.shutdown().await;
    }
    surface_result
}

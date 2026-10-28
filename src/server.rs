//! The rmcp server: tool surface and handler dispatch.
//!
//! Domain logic does not move in the WS2 migration — the sixteen
//! tool handlers are the hand-rolled server's, reached through the
//! same dispatch the baseline's front end ran (see the retired
//! `mcp.rs` contract, now carried here): attention tools are served
//! from the in-process coordination board before any broker
//! connection is opened; the broker connects lazily on the first
//! brokered call and is retried per call while it stays unavailable;
//! a tools/call is answered in-band (tool failures are results with
//! `isError: true`, never protocol errors). Only the framing changes
//! sides of the seam: rmcp owns the envelope, the era guard (see
//! `guard.rs`) owns the stdio front door, and this type owns the
//! surface.
//!
//! Two era translations live here, both forced by the modern
//! protocol's statelessness and both invisible on the wire:
//!
//! - **Caller identity.** The baseline bound attention publishes to
//!   the `clientInfo.name` captured at `initialize`. The modern era
//!   has no handshake; the same identity arrives per request in
//!   `_meta` (`io.modelcontextprotocol/clientInfo`), stamped by
//!   every modern client, and is read from the request's typed
//!   metadata. Absent identity fails closed exactly as before
//!   ("attention tools require an identified client").
//! - **Dispatch serialization.** The baseline's front end was a
//!   sequential loop — one call at a time, broker state behind it.
//!   rmcp dispatches concurrently, so dispatch here takes the state
//!   mutex for the whole call, preserving the baseline's ordering
//!   and its lazy-connect semantics (no double-connect race).
//!
//! Modern-only posture lives in two places: the era guard refuses
//! `initialize` before the SDK sees it, and
//! [`ServerHandler::supported_protocol_versions`] pins this handler
//! to 2026-07-28, so `server/discover` advertises exactly one
//! version and the SDK validates every request's `_meta` against it.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::attention;
use crate::broker::{BrokerConfig, NvimBroker};
use crate::tools;

/// The tool catalog: names, descriptions, and input schemas are the
/// baseline's own definitions, byte-for-byte — rmcp accepts raw
/// schemas, so no schemars translation stands between the baseline
/// and the wire.
fn tool_definitions() -> Vec<Tool> {
    let definitions = tools::definitions();
    let entries = definitions.as_array().expect("definitions are an array");
    entries
        .iter()
        .map(|entry| {
            let name = entry["name"].as_str().expect("tool name").to_string();
            let description = entry["description"]
                .as_str()
                .expect("tool description")
                .to_string();
            let schema = entry["inputSchema"]
                .as_object()
                .expect("input schema is an object")
                .clone();
            Tool::new(name, description, schema)
        })
        .collect()
}

fn text_result(text: String, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// The baseline's in-band tool error: plain message text.
fn tool_error(message: &str) -> CallToolResult {
    text_result(message.to_string(), true)
}

/// Mutable server state behind the dispatch mutex: the lazily
/// connected broker (the baseline connected on first use and kept
/// the connection for the process's life).
struct State {
    broker: Option<NvimBroker>,
    broker_config: BrokerConfig,
}

/// The server handler. Owns the broker lifecycle the baseline's
/// front end owned: connect lazily, shut the broker down when the
/// serve ends (see `main.rs`). A clonable handle over shared state,
/// so the serve can own one clone while `main` keeps another for
/// the shutdown.
#[derive(Clone)]
pub struct PeriscopeServer {
    state: Arc<Mutex<State>>,
}

impl PeriscopeServer {
    pub fn new(broker_config: BrokerConfig) -> Self {
        PeriscopeServer {
            state: Arc::new(Mutex::new(State {
                broker: None,
                broker_config,
            })),
        }
    }

    /// Shut the broker down, if one was ever connected (managed
    /// editors are quit and reaped; attached editors are only
    /// disconnected from). Called once the serve has ended.
    pub async fn shutdown_broker(&self) {
        let broker = self.state.lock().await.broker.take();
        if let Some(broker) = broker {
            broker.shutdown().await;
        }
    }

    /// The baseline's `call_tool`, unchanged: attention first (it
    /// never touches the editor), then the lazy broker connect,
    /// then the tool dispatch. Caller identity is the transport
    /// identity of this request.
    async fn dispatch(
        state: &mut State,
        caller: Option<&str>,
        name: &str,
        arguments: &Value,
    ) -> CallToolResult {
        if name.starts_with("attention_") {
            return match attention::call(caller, name, arguments) {
                Ok(result) => text_result(result.to_string(), false),
                Err(error) => tool_error(&error.to_string()),
            };
        }
        if state.broker.is_none() {
            match NvimBroker::connect(state.broker_config.clone()).await {
                Ok(broker) => state.broker = Some(broker),
                Err(error) => {
                    return tool_error(&format!("connecting to neovim failed: {error}"));
                }
            }
        }
        let broker = state.broker.as_ref().expect("broker checked above");
        match tools::call(broker, broker.mode(), name, arguments).await {
            Ok(result) => text_result(result.to_string(), false),
            Err(error) => tool_error(&error.to_string()),
        }
    }
}

/// The request's transport identity: the client name from the
/// per-request `_meta` clientInfo, the modern era's successor to
/// the name the baseline captured from `initialize`. (rmcp carries
/// the request `_meta` on the request context, not on the params.)
fn caller_identity(context: &RequestContext<RoleServer>) -> Option<String> {
    context.meta.client_info().map(|info| info.name)
}

impl ServerHandler for PeriscopeServer {
    fn get_info(&self) -> InitializeResult {
        // Both model types are non-exhaustive: start from the
        // default and set exactly the fields this server reports.
        let mut server_info = Implementation::default();
        server_info.name = "periscope".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tool_definitions(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = caller_identity(&context);
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        // The baseline served sequentially; hold the state mutex
        // for the whole dispatch so broker state, the lazy connect,
        // and the attention board see the same ordering.
        let mut state = self.state.lock().await;
        let result = Self::dispatch(&mut state, caller.as_deref(), &name, &arguments).await;
        Ok(result.into())
    }
}

/// The wire-shape smoke: the catalog the handler serves is the
/// baseline's definitions, in order. (Kept beside the conversion it
/// guards; the integration suite checks the same over the wire.)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_matches_the_baseline_definitions() {
        let served: Vec<String> = tool_definitions()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let baseline: Vec<String> = tools::definitions()
            .as_array()
            .expect("array")
            .iter()
            .map(|entry| entry["name"].as_str().expect("name").to_string())
            .collect();
        assert_eq!(served, baseline);
        assert_eq!(served.len(), 16);
        assert_eq!(served[0], "attention_alignment");
        assert_eq!(served[15], "run_lint");
    }

    #[test]
    fn catalog_schemas_are_the_baselines_own() {
        let served = serde_json::json!(tool_definitions());
        let baseline = tools::definitions();
        let served_entries = served.as_array().expect("array");
        let baseline_entries = baseline.as_array().expect("array");
        for (served, baseline) in served_entries.iter().zip(baseline_entries) {
            assert_eq!(served["inputSchema"], baseline["inputSchema"]);
            assert_eq!(served["description"], baseline["description"]);
        }
    }
}

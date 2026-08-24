//! A generic, reusable MCP (Model Context Protocol) server shell for
//! local-first personal-data apps — contacts/company CRMs, or anything else
//! shaped roughly like one.
//!
//! This crate has zero knowledge of any particular app's schema or storage.
//! It defines [`McpBackend`], a trait describing the six operations a
//! contacts-style data source needs to support, and [`McpServer`], a generic
//! MCP server that exposes those operations as MCP tools over any transport
//! `rmcp` supports (stdio, HTTP/SSE, …). Bring your own backend by
//! implementing [`McpBackend`] against your own data; this crate handles the
//! MCP protocol plumbing, tool schemas, and descriptions.
//!
//! This is the extracted, generic core of Proactive Vault's own MCP
//! integration (proactivepotential.com) — Vault's private app implements
//! [`McpBackend`] against its real SQLite-backed contacts/company store and
//! depends on this crate directly, so its standalone stdio binary keeps
//! working with zero GUI dependency, exactly as before the split.

use async_trait::async_trait;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    ErrorData, ServerHandler,
};
use serde::{Deserialize, Serialize};

/// The six operations a contacts/company-style backend must implement to be
/// exposed as MCP tools. Every method takes `&self` (implementers own their
/// own interior mutability/locking, e.g. an `Arc<Mutex<Connection>>`) and
/// returns `Result<_, Self::Error>` — errors are surfaced to the calling
/// agent as an MCP tool error, via `Self::Error: Display`.
///
/// `Contact`/`Company`/`Stats` are associated types, not fixed structs —
/// each backend defines its own shape. This crate only needs them to be
/// `Serialize` (to return as tool output); it never inspects their fields
/// itself. Only tool *inputs* (defined by this crate, not the backend) need
/// a JSON schema — tool outputs are just serialized directly.
#[async_trait]
pub trait McpBackend: Send + Sync + 'static {
    type Contact: Serialize + Send + Sync;
    type Company: Serialize + Send + Sync;
    type Stats: Serialize + Send + Sync;
    type Error: std::fmt::Display + Send + Sync;

    /// Free-text search across whatever fields the backend considers
    /// relevant (name, company, title, notes, …). `status` is a
    /// backend-defined filter string (Vault uses "active" | "archived" |
    /// "all"; a backend with no such concept can just ignore it). Returns
    /// `(total_matches, this_page)` — `total_matches` is the FULL count for
    /// the query/status filter, not just `this_page.len()`, so an agent can
    /// tell a capped page apart from the true total.
    async fn search_contacts(
        &self,
        query: &str,
        status: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<Self::Contact>), Self::Error>;

    /// Aggregate counts (however the backend defines "a contact" / "a
    /// company") — cheaper and unambiguous compared to paging through
    /// `search_contacts` with an empty query just to count rows.
    async fn stats(&self) -> Result<Self::Stats, Self::Error>;

    /// Look up a company by name (case-insensitive is recommended, not
    /// required) and return it plus every contact considered "at" it.
    /// `found: false` (not an error) when nothing matches.
    async fn company_network(
        &self,
        name: &str,
    ) -> Result<(bool, Option<Self::Company>, Vec<Self::Contact>), Self::Error>;

    /// Update one field on a contact. `field` is a plain backend-defined
    /// name (e.g. "email", "position") — this crate has no fixed field
    /// list; a backend should return `Self::Error` for an unknown or
    /// disallowed field rather than silently ignoring it.
    async fn update_contact_field(
        &self,
        contact_id: i64,
        field: &str,
        value: &str,
    ) -> Result<Self::Contact, Self::Error>;

    /// Rename a company, or (if `new_name` already belongs to a different
    /// company) merge `company_id` into it — reparenting every contact at
    /// `company_id` either way, so a rename/merge never silently orphans a
    /// contact roster. Consequential and not easily undone; backends should
    /// document their own merge semantics for callers.
    async fn merge_company_alias(
        &self,
        company_id: i64,
        new_name: &str,
    ) -> Result<Self::Company, Self::Error>;

    /// Flag contacts for manual follow-up. This crate has no opinion on what
    /// happens next — a backend may treat this as a bookmark list (Vault
    /// does), a real job queue, or anything else. Returns how many were
    /// queued.
    async fn enqueue_enrichment(&self, contact_ids: &[i64]) -> Result<i64, Self::Error>;
}

fn backend_err<E: std::fmt::Display>(e: E) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

/// Accepts a JSON number OR a numeric string (some MCP clients serialize
/// every tool-call argument as a string regardless of the declared schema
/// type) and parses either into an `i64`. `null`/absent falls through to the
/// field's `#[serde(default)]`.
fn deserialize_flexible_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => {
            n.as_i64().map(Some).ok_or_else(|| D::Error::custom(format!("expected an integer, got {n}")))
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<i64>()
                    .map(Some)
                    .map_err(|_| D::Error::custom(format!("expected an integer, got string {trimmed:?}")))
            }
        }
        other => Err(D::Error::custom(format!("expected an integer or numeric string, got {other}"))),
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchContactsParams {
    /// Free-text query (interpretation is backend-defined — Vault matches
    /// name/company/position/notes as AND-ed prefix terms).
    query: String,
    /// Backend-defined status filter (e.g. "active" | "archived" | "all").
    /// Omit for the backend's own default.
    #[serde(default)]
    status: Option<String>,
    /// Max rows to return. Default 25, capped at 200. Accepts a number or a
    /// numeric string.
    #[serde(default, deserialize_with = "deserialize_flexible_i64")]
    limit: Option<i64>,
    /// Rows to skip before collecting `limit` results. Default 0. Accepts a
    /// number or a numeric string.
    #[serde(default, deserialize_with = "deserialize_flexible_i64")]
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SearchContactsResult<C> {
    total_matches: i64,
    results: Vec<C>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompanyNetworkParams {
    /// Company name to look up.
    name: String,
}

#[derive(Debug, Serialize)]
struct CompanyNetworkResult<Co, Ct> {
    found: bool,
    company: Option<Co>,
    contacts: Vec<Ct>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateContactFieldParams {
    contact_id: i64,
    /// Backend-defined field name (e.g. "email", "position"). An unknown or
    /// disallowed field is rejected by the backend, not this crate.
    field: String,
    value: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MergeCompanyAliasParams {
    company_id: i64,
    new_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EnqueueEnrichmentParams {
    contact_ids: Vec<i64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct EnqueueResult {
    queued: i64,
}

/// The generic MCP server. Wraps any [`McpBackend`] implementation and
/// exposes its six operations as MCP tools. Construct one with
/// [`McpServer::new`], then hand it to whichever `rmcp` transport you want
/// (see [`McpServer::serve_stdio`] for the common stdio case, or use
/// `rmcp::ServiceExt::serve` directly for HTTP/SSE and other transports).
#[derive(Clone)]
pub struct McpServer<B: McpBackend> {
    backend: std::sync::Arc<B>,
    server_name: &'static str,
    instructions: &'static str,
}

impl<B: McpBackend> McpServer<B> {
    /// `server_name` and `instructions` are surfaced to the connecting MCP
    /// client (e.g. shown in Claude Desktop's server list / used by the
    /// model to decide when to reach for these tools) — pick something that
    /// describes your own app, not this crate.
    pub fn new(backend: B, server_name: &'static str, instructions: &'static str) -> Self {
        Self { backend: std::sync::Arc::new(backend), server_name, instructions }
    }
}

#[tool_router]
impl<B: McpBackend> McpServer<B> {
    #[tool(
        description = "Search contacts by free text across name, company, position, and notes. \
                        The response's `total_matches` is the FULL count for this query/status \
                        filter — `results` is only the page selected by `limit`/`offset` (default \
                        25, capped at 200), so `results.len()` is almost always smaller than \
                        `total_matches`. Use `offset` to page through the rest. For a total count \
                        with NO query filter, use get_stats instead."
    )]
    async fn search_contacts(
        &self,
        Parameters(p): Parameters<SearchContactsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = p.limit.unwrap_or(25).clamp(1, 200);
        let offset = p.offset.unwrap_or(0).max(0);
        let status = p.status.as_deref().unwrap_or("active");
        let (total, results) =
            self.backend.search_contacts(&p.query, status, limit, offset).await.map_err(backend_err)?;
        json_result(&SearchContactsResult { total_matches: total, results })
    }

    #[tool(
        description = "Get aggregate counts (contacts, companies) for the whole data source. Use \
                        this instead of search_contacts when asked for total numbers — \
                        search_contacts only ever returns a limited page of results."
    )]
    async fn get_stats(&self) -> Result<CallToolResult, ErrorData> {
        let stats = self.backend.stats().await.map_err(backend_err)?;
        json_result(&stats)
    }

    #[tool(
        description = "Look up a company by name and return it plus every contact at that \
                        company. `found: false` (not an error) when no company matches that name."
    )]
    async fn get_company_network(
        &self,
        Parameters(p): Parameters<CompanyNetworkParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (found, company, contacts) = self.backend.company_network(&p.name).await.map_err(backend_err)?;
        json_result(&CompanyNetworkResult { found, company, contacts })
    }

    #[tool(
        description = "Update one field on a contact (e.g. inject an email address discovered \
                        elsewhere). Which fields are allowed, and any collision/validation rules, \
                        are defined by the backend — an unsupported field is refused, not silently \
                        dropped."
    )]
    async fn update_contact_field(
        &self,
        Parameters(p): Parameters<UpdateContactFieldParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let updated = self
            .backend
            .update_contact_field(p.contact_id, &p.field, &p.value)
            .await
            .map_err(backend_err)?;
        json_result(&updated)
    }

    #[tool(
        description = "Rename a company, or merge it into an existing company of the target name \
                        if one already exists. Every contact at `company_id` is reparented onto \
                        the new name either way. This is consequential and not easily undone — use \
                        get_company_network first to confirm which company is which before calling."
    )]
    async fn merge_company_alias(
        &self,
        Parameters(p): Parameters<MergeCompanyAliasParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let merged =
            self.backend.merge_company_alias(p.company_id, &p.new_name).await.map_err(backend_err)?;
        json_result(&merged)
    }

    #[tool(
        description = "Flag contacts for manual follow-up. This is a bookmark list, not a job \
                        queue — nothing runs on a flagged contact automatically."
    )]
    async fn enqueue_enrichment(
        &self,
        Parameters(p): Parameters<EnqueueEnrichmentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let queued = self.backend.enqueue_enrichment(&p.contact_ids).await.map_err(backend_err)?;
        json_result(&EnqueueResult { queued })
    }
}

#[tool_handler]
impl<B: McpBackend> ServerHandler for McpServer<B> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(self.server_name, env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions)
    }
}

impl<B: McpBackend> McpServer<B> {
    /// Convenience wrapper around `rmcp`'s stdio transport — the common case
    /// for a standalone binary an MCP client (Claude Desktop, etc.) spawns
    /// directly. Blocks until the client disconnects.
    pub async fn serve_stdio(self) -> Result<(), Box<dyn std::error::Error>> {
        use rmcp::{transport::stdio, ServiceExt};
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }
}

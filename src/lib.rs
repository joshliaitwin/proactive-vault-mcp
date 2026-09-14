//! A generic, reusable MCP (Model Context Protocol) server shell for
//! local-first personal-data apps — contacts/company CRMs, or anything else
//! shaped roughly like one.
//!
//! This crate has zero knowledge of any particular app's schema or storage.
//! It defines [`McpBackend`], a trait describing the eight operations a
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

/// The eight operations a contacts/company-style backend must implement to
/// be exposed as MCP tools. Every method takes `&self` (implementers own
/// their own interior mutability/locking, e.g. an `Arc<Mutex<Connection>>`)
/// and returns `Result<_, Self::Error>` — errors are surfaced to the calling
/// agent as an MCP tool error, via `Self::Error: Display`.
///
/// `Contact`/`Company`/`Stats` are associated types, not fixed structs —
/// each backend defines its own shape. This crate only needs them to be
/// `Serialize` (to return as tool output); it never inspects their fields
/// itself. Only tool *inputs* (defined by this crate, not the backend) need
/// a JSON schema — tool outputs are just serialized directly.
///
/// `Error: From<&'static str>` (in addition to `Display`) exists so the two
/// *default-implemented* methods below (`update_company_fields`,
/// `list_companies_needing_enrichment`) can construct a "not supported by
/// this backend" error generically, without requiring every implementer to
/// override them. This is a small additional bound beyond the original
/// `Display`-only requirement — checked against Vault's own `Error = String`
/// (satisfied: `String: From<&str>` for any lifetime including `'static`)
/// and reasonable for any other implementer, since almost every error type
/// either already has this impl or can derive it trivially (e.g. via
/// `thiserror`'s `#[error("{0}")]` on a newtype, or a plain `String`/`Box<dyn
/// Error>` error type).
#[async_trait]
pub trait McpBackend: Send + Sync + 'static {
    type Contact: Serialize + Send + Sync;
    type Company: Serialize + Send + Sync;
    type Stats: Serialize + Send + Sync;
    type Error: std::fmt::Display + Send + Sync + From<&'static str>;

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

    /// Update a subset of a company's enrichment fields — whichever the
    /// backend supports (Vault's whitelist: url, industry, category,
    /// size_range, description, hq_location, stock_symbol, main_phone).
    /// `overwrite`: when false (the default a caller should pass unless they
    /// mean to correct something), a field with a non-blank existing value is
    /// left untouched even if a new value is supplied — the backend reports
    /// which fields were actually written. When true, every supplied
    /// non-empty field is written regardless of its current value.
    /// Default implementation returns `Self::Error` — override to support
    /// this operation; backends that don't are simply not enrichable via MCP.
    async fn update_company_fields(
        &self,
        _company_id: i64,
        _fields: CompanyFieldUpdates,
        _overwrite: bool,
    ) -> Result<CompanyFieldsWriteResult<Self::Company>, Self::Error> {
        Err("update_company_fields is not supported by this backend".into())
    }

    /// Companies missing enrichment data (backend-defined "missing" — Vault's
    /// is a blank `url`), sorted by contact count descending, with obvious
    /// non-company placeholder names excluded. `limit` capped by the backend
    /// (Vault: default 50, max 500). Read-only.
    /// Default implementation returns an empty list — override to support.
    async fn list_companies_needing_enrichment(
        &self,
        _limit: i64,
    ) -> Result<Vec<Self::Company>, Self::Error> {
        Ok(vec![])
    }

    /// Update a subset of a contact's enrichment fields — whichever the
    /// backend supports (Vault's whitelist: schools_attended,
    /// previous_companies, geographic_location, ai_research_summary).
    /// `write_mode` is backend-defined; Vault accepts "overwrite" (replace
    /// unconditionally), "skip" (the default — leave a field alone if it
    /// already has a value), or "append" (add the new value after the
    /// existing one, with a date stamp, rather than replacing it).
    /// `ai_research_summary` is a system-authored synthesis, not a place a
    /// user hand-types notes, so a backend may choose to always overwrite it
    /// regardless of `write_mode` — see the backend's own docs.
    /// Default implementation returns `Self::Error` — override to support
    /// this operation; backends that don't are simply not enrichable via MCP.
    async fn update_contact_fields(
        &self,
        _contact_id: i64,
        _fields: ContactFieldUpdates,
        _write_mode: &str,
    ) -> Result<ContactFieldsWriteResult<Self::Contact>, Self::Error> {
        Err("update_contact_fields is not supported by this backend".into())
    }

    /// Contacts missing enrichment data (backend-defined "missing"). `limit`
    /// capped by the backend (Vault: default 50, max 200). Read-only.
    /// Default implementation returns an empty list — override to support.
    async fn list_contacts_needing_enrichment(
        &self,
        _limit: i64,
    ) -> Result<Vec<Self::Contact>, Self::Error> {
        Ok(vec![])
    }

    /// Exports a full, still-encrypted snapshot of the ENTIRE data source
    /// (every record, every field — not just what the enrichment tools
    /// touch) to `path`, a destination the caller chooses. This is a
    /// whole-database backup, meant to be safe to call unattended on a
    /// schedule: it must never modify the live data, only read from it. The
    /// resulting file's exact requirements for being opened again (does it
    /// need the same encryption key, the same app, …) are entirely
    /// backend-defined — this crate has no opinion on storage format.
    /// Default implementation returns `Self::Error` — override to support
    /// this; a backend with no on-disk database concept, or that doesn't
    /// want to expose this over MCP, simply doesn't implement it.
    async fn backup_vault(&self, _path: &str) -> Result<(), Self::Error> {
        Err("backup_vault is not supported by this backend".into())
    }
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

/// One entry per Vault-whitelisted enrichment field. All optional — supply
/// only the ones you have a value for.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CompanyFieldUpdates {
    /// Official website, e.g. "https://www.acme.com".
    #[serde(default)]
    pub url: Option<String>,
    /// Free-text business sector, e.g. "Banking & Financial Services".
    #[serde(default)]
    pub industry: Option<String>,
    /// The kind of organisation. EXACTLY ONE of: "Public Company",
    /// "Private Company", "Academic Institution", "Non-Profit",
    /// "Government Entity", "Partnership", "Subsidiary". Rules of thumb:
    /// has a stock ticker → Public Company; .edu domain or a
    /// university/college/school → Academic Institution; .gov/.mil → Government
    /// Entity; .org → Non-Profit; a law firm / LLP / PwC / Deloitte / EY / KPMG
    /// → Partnership; plainly owned by another company → Subsidiary; otherwise
    /// → Private Company.
    #[serde(default)]
    pub category: Option<String>,
    /// Employee count as a single number, e.g. "74000".
    #[serde(default)]
    pub size_range: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hq_location: Option<String>,
    #[serde(default)]
    pub stock_symbol: Option<String>,
    #[serde(default)]
    pub main_phone: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct UpdateCompanyFieldsParams {
    company_id: i64,
    #[serde(default)]
    fields: CompanyFieldUpdates,
    /// If true, overwrite fields that already have a value. Default false —
    /// only fills in currently-blank fields.
    #[serde(default)]
    overwrite: bool,
}

/// Result of `update_company_fields` — reports what actually changed, not
/// just the resulting record, so an agent (and the user watching the
/// permission prompt) can see exactly what was written vs. left alone.
#[derive(Debug, Serialize)]
pub struct CompanyFieldsWriteResult<Co> {
    pub company: Co,
    /// Field names actually written this call.
    pub fields_written: Vec<&'static str>,
    /// Field names supplied but skipped because they already had a value
    /// and `overwrite` was false.
    pub fields_skipped_already_set: Vec<&'static str>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListCompaniesNeedingEnrichmentParams {
    /// Max companies to return. Default 50, capped at 500. Accepts a number
    /// or a numeric string.
    #[serde(default, deserialize_with = "deserialize_flexible_i64")]
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ListCompaniesNeedingEnrichmentResult<Co> {
    companies: Vec<Co>,
}

/// One entry per Vault-whitelisted contact enrichment field. All optional —
/// supply only the ones you have a value for. `deny_unknown_fields`
/// deliberately rejects, rather than silently drops, a field that belongs
/// elsewhere (e.g. "email", which is core-column data set via the separate
/// `update_contact_field` tool, not a member of this struct) — an agent that
/// puts it here by mistake gets a loud, actionable schema error instead of a
/// tool call that reports success while quietly writing nothing for that
/// field (confirmed live, 2026-09-12: an agent claimed it injected a primary
/// email this way and the value never reached the vault).
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContactFieldUpdates {
    /// Free text, as found (e.g. "Harvard University (MBA); UC Berkeley (BS)").
    #[serde(default)]
    pub schools_attended: Option<String>,
    /// Free-text list of past employers — from the profile's Experience
    /// section (check it explicitly; don't rely on About text alone). If
    /// there's no Experience section at all, fall back to whatever role/
    /// company info is stated in the About text instead.
    #[serde(default)]
    pub previous_companies: Option<String>,
    /// Free-text list of past job titles, paired with previous_companies —
    /// from the same Experience section (e.g. "VP Engineering at Acme;
    /// Senior Engineer at Beta Corp").
    #[serde(default)]
    pub previous_positions: Option<String>,
    /// City, state, country if available; else region/country; else country
    /// only. Never more precise than city-level.
    #[serde(default)]
    pub geographic_location: Option<String>,
    /// A fresh 2-4 sentence synthesis (LinkedIn "About" + recent activity, or
    /// a general web search). Rewritten each enrichment run, not appended to.
    #[serde(default)]
    pub ai_research_summary: Option<String>,
    /// Mobile phone number, as found (e.g. from LinkedIn's "Contact info"
    /// overlay — NOT the public profile page, which never shows it). Use
    /// this specifically for a number LinkedIn itself labels "Mobile" —
    /// "Home"/"Work" go in home_phone/business_phone instead. LinkedIn lets
    /// someone list more than one phone of the SAME type; if so, join them
    /// (e.g. "6825835401; 2145550100").
    #[serde(default)]
    pub mobile_phone: Option<String>,
    /// A phone number LinkedIn itself labels "Home".
    #[serde(default)]
    pub home_phone: Option<String>,
    /// A phone number LinkedIn itself labels "Work".
    #[serde(default)]
    pub business_phone: Option<String>,
    /// Street address, as found (LinkedIn "Contact info" overlay only).
    #[serde(default)]
    pub home_address: Option<String>,
    /// Website(s) from the "Contact info" overlay, each with its LinkedIn
    /// type. Format as "Type: url" per entry, joined with "; " if there's
    /// more than one (e.g. "Company: https://roxe.io; Personal: https://joshli.com").
    #[serde(default)]
    pub websites: Option<String>,
    /// Instant-messaging handle(s) from the "Contact info" overlay. Format
    /// as "Service: username" per entry, joined with "; " if there's more
    /// than one (e.g. "Skype: jdoe123; WeChat: jdoe_wc").
    #[serde(default)]
    pub instant_message: Option<String>,
    /// Birthday, as found (e.g. "November 22" from LinkedIn's "Contact info"
    /// overlay). A backend may ignore `write_mode: "append"` for this field
    /// (a date has no sensible "append" form) and treat it as "overwrite".
    #[serde(default)]
    pub birthday: Option<String>,
    /// A personal/secondary email discovered during research (e.g. from
    /// LinkedIn's "Contact info" overlay), DISTINCT from the contact's
    /// primary `email`. A backend may add this to a list rather than
    /// replacing it outright, regardless of `write_mode` — see its own docs.
    #[serde(default)]
    pub secondary_email: Option<String>,
}

/// `deny_unknown_fields` here too — a field like "email" put at the top
/// level (a sibling of `fields`, rather than nested inside it) should also
/// error loudly rather than being silently dropped. See `ContactFieldUpdates`'s
/// own doc comment for why this matters.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateContactFieldsParams {
    contact_id: i64,
    #[serde(default)]
    fields: ContactFieldUpdates,
    /// How to handle a field that already has a value: "overwrite" (replace
    /// it), "skip" (leave it alone — the default), or "append" (add the new
    /// value after the existing one with a date stamp). Backend-defined
    /// beyond these three conventional values.
    #[serde(default)]
    write_mode: Option<String>,
}

/// Result of `update_contact_fields` — reports what actually changed, not
/// just the resulting record.
#[derive(Debug, Serialize)]
pub struct ContactFieldsWriteResult<Ct> {
    pub contact: Ct,
    /// Field names actually written this call.
    pub fields_written: Vec<&'static str>,
    /// Field names supplied but skipped because they already had a value and
    /// `write_mode` was "skip" (or omitted).
    pub fields_skipped_already_set: Vec<&'static str>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListContactsNeedingEnrichmentParams {
    /// Max contacts to return. Default 50, capped at 200. Accepts a number or
    /// a numeric string.
    #[serde(default, deserialize_with = "deserialize_flexible_i64")]
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ListContactsNeedingEnrichmentResult<Ct> {
    contacts: Vec<Ct>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BackupVaultParams {
    /// Absolute destination file path for the backup, e.g.
    /// "/Users/jane/Library/Mobile Documents/com~apple~CloudDocs/vault-backup-2026-09-14-0600.db".
    /// The parent directory must already exist — this does not create
    /// folders. If a file already exists at this exact path, it's
    /// overwritten.
    path: String,
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

    #[tool(
        description = "Update a company's enrichment fields (url, industry, category, \
                        size_range, description, hq_location, stock_symbol, main_phone) — any \
                        subset you have values for. `category` must be EXACTLY ONE of: \
                        \"Public Company\", \"Private Company\", \"Academic Institution\", \
                        \"Non-Profit\", \"Government Entity\", \"Partnership\", \"Subsidiary\" \
                        (has a ticker → Public Company; .edu / a school → Academic Institution; \
                        .gov/.mil → Government Entity; .org → Non-Profit; law firm / LLP / Big \
                        Four → Partnership; owned by another company → Subsidiary; else → \
                        Private Company). By default only fills fields that are currently \
                        blank; a field that already has a value is left untouched unless you \
                        pass overwrite: true. The result tells you which fields were actually \
                        written vs. skipped because they were already set. Use \
                        get_company_network first if you're not certain of the company_id."
    )]
    async fn update_company_fields(
        &self,
        Parameters(p): Parameters<UpdateCompanyFieldsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .backend
            .update_company_fields(p.company_id, p.fields, p.overwrite)
            .await
            .map_err(backend_err)?;
        json_result(&result)
    }

    #[tool(
        description = "List companies missing enrichment data (blank domain/industry/etc.), \
                        sorted by contact count descending, with obvious non-company LinkedIn \
                        export artifacts (\"Self Employed\", \"Freelance\", \"Consultant\", \
                        \"Stealth ...\", and similar) already excluded. Read-only — use this to \
                        pick a batch to enrich, then update_company_fields for each one. Default \
                        50, capped at 500."
    )]
    async fn list_companies_needing_enrichment(
        &self,
        Parameters(p): Parameters<ListCompaniesNeedingEnrichmentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = p.limit.unwrap_or(50).clamp(1, 500);
        let companies = self.backend.list_companies_needing_enrichment(limit).await.map_err(backend_err)?;
        json_result(&ListCompaniesNeedingEnrichmentResult { companies })
    }

    #[tool(
        description = "Update a contact's enrichment fields — any subset you have values for: \
                        schools_attended, previous_companies, previous_positions, \
                        geographic_location, ai_research_summary, mobile_phone, home_phone, \
                        business_phone, home_address, websites, instant_message, birthday, \
                        secondary_email. previous_companies/previous_positions come from the \
                        profile's Experience section specifically (check it explicitly) — fall back \
                        to the About text only when there's no Experience section at all. \
                        geographic_location should never be more precise than city-level (city, \
                        state, country if available; else region/country; else country only — no \
                        street address; a full street address, if you have one, is home_address \
                        instead). ai_research_summary is a fresh 2-4 sentence synthesis, rewritten \
                        each run, not appended to. mobile_phone/home_phone/business_phone/ \
                        home_address/websites/instant_message/birthday are typically only visible \
                        on LinkedIn's \"Contact info\" overlay, not the public profile page — match \
                        each phone to the type LinkedIn itself labels it (Mobile/Home/Work); for \
                        websites and instant_message, format each entry as \"Type: value\" and join \
                        multiple with \"; \". secondary_email is a personal/alternate email distinct \
                        from the contact's primary email — it's added to a list rather than \
                        replacing anything, regardless of write_mode. write_mode controls what \
                        happens to a field that already has a value: \"skip\" (default — leave it \
                        alone), \"overwrite\" (replace it), or \"append\" (add the new value after \
                        the old one with a date stamp; for birthday, treated as \"overwrite\" since \
                        a date has no sensible append form). The result tells you which fields were \
                        actually written vs. skipped. Use search_contacts first if you're not \
                        certain of the contact_id."
    )]
    async fn update_contact_fields(
        &self,
        Parameters(p): Parameters<UpdateContactFieldsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let write_mode = p.write_mode.as_deref().unwrap_or("skip");
        let result = self
            .backend
            .update_contact_fields(p.contact_id, p.fields, write_mode)
            .await
            .map_err(backend_err)?;
        json_result(&result)
    }

    #[tool(
        description = "List contacts missing enrichment data (schools_attended, previous_companies, \
                        geographic_location, or ai_research_summary all blank). Read-only — use this \
                        to pick a batch to enrich, then update_contact_fields for each one. Default \
                        50, capped at 200."
    )]
    async fn list_contacts_needing_enrichment(
        &self,
        Parameters(p): Parameters<ListContactsNeedingEnrichmentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = p.limit.unwrap_or(50).clamp(1, 200);
        let contacts = self.backend.list_contacts_needing_enrichment(limit).await.map_err(backend_err)?;
        json_result(&ListContactsNeedingEnrichmentResult { contacts })
    }

    #[tool(
        description = "Export a full, still-encrypted backup of the ENTIRE vault (every contact, \
                        company, and custom field — not a per-record write) to `path`. The parent \
                        directory must already exist. Safe to call on an unattended schedule: this \
                        only reads a snapshot of the live data, it never modifies anything. The \
                        resulting file typically only opens again on the same machine/install it \
                        was made on — check the backend's own instructions or documentation before \
                        assuming a backup can be moved elsewhere and restored."
    )]
    async fn backup_vault(
        &self,
        Parameters(p): Parameters<BackupVaultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.backend.backup_vault(&p.path).await.map_err(backend_err)?;
        json_result(&serde_json::json!({ "backed_up_to": p.path }))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the 2026-09-12 silent-drop incident: an agent put
    /// "email" inside update_contact_fields' `fields` object (it belongs to
    /// the separate update_contact_field tool) and the call reported success
    /// while the value never reached the backend at all. `deny_unknown_fields`
    /// must turn that into a loud deserialization error instead.
    #[test]
    fn email_inside_fields_object_is_rejected_not_silently_dropped() {
        let raw = r#"{"contact_id": 1, "fields": {"email": "x@example.com"}}"#;
        let err = serde_json::from_str::<UpdateContactFieldsParams>(raw)
            .expect_err("an unknown field inside `fields` must error, not silently ignore the value");
        assert!(err.to_string().contains("email"), "error should name the offending field: {err}");
    }

    /// Same protection at the top level — "email" as a sibling of `fields`
    /// rather than nested inside it.
    #[test]
    fn email_at_top_level_is_also_rejected() {
        let raw = r#"{"contact_id": 1, "email": "x@example.com"}"#;
        let err = serde_json::from_str::<UpdateContactFieldsParams>(raw)
            .expect_err("an unknown top-level field must error, not silently ignore the value");
        assert!(err.to_string().contains("email"), "error should name the offending field: {err}");
    }

    #[derive(Debug, Clone, Serialize)]
    struct FakeContact {
        id: i64,
    }
    #[derive(Debug, Clone, Serialize)]
    struct FakeCompany {
        id: i64,
        name: String,
    }
    #[derive(Debug, Clone, Serialize)]
    struct FakeStats {
        total: i64,
    }

    /// Implements every ORIGINAL required `McpBackend` method but
    /// deliberately does NOT override `update_company_fields` or
    /// `list_companies_needing_enrichment` — this is the actual proof that
    /// adding those two methods (both default-implemented) did not break an
    /// existing implementer, which is the whole point of making them
    /// default-implemented rather than required.
    struct MinimalBackend;

    #[async_trait]
    impl McpBackend for MinimalBackend {
        type Contact = FakeContact;
        type Company = FakeCompany;
        type Stats = FakeStats;
        type Error = String;

        async fn search_contacts(
            &self,
            _query: &str,
            _status: &str,
            _limit: i64,
            _offset: i64,
        ) -> Result<(i64, Vec<Self::Contact>), Self::Error> {
            Ok((0, vec![]))
        }

        async fn stats(&self) -> Result<Self::Stats, Self::Error> {
            Ok(FakeStats { total: 0 })
        }

        async fn company_network(
            &self,
            _name: &str,
        ) -> Result<(bool, Option<Self::Company>, Vec<Self::Contact>), Self::Error> {
            Ok((false, None, vec![]))
        }

        async fn update_contact_field(
            &self,
            _contact_id: i64,
            _field: &str,
            _value: &str,
        ) -> Result<Self::Contact, Self::Error> {
            Ok(FakeContact { id: 1 })
        }

        async fn merge_company_alias(&self, _company_id: i64, _new_name: &str) -> Result<Self::Company, Self::Error> {
            Ok(FakeCompany { id: 1, name: "x".to_string() })
        }

        async fn enqueue_enrichment(&self, contact_ids: &[i64]) -> Result<i64, Self::Error> {
            Ok(contact_ids.len() as i64)
        }

        // `update_company_fields` / `list_companies_needing_enrichment` /
        // `update_contact_fields` / `list_contacts_needing_enrichment`:
        // intentionally NOT overridden — see `MinimalBackend`'s doc comment.
    }

    #[tokio::test]
    async fn default_update_contact_fields_returns_a_clear_not_supported_error() {
        let backend = MinimalBackend;
        let err = backend
            .update_contact_fields(1, ContactFieldUpdates::default(), "skip")
            .await
            .expect_err("a backend that doesn't override this must reject it, not silently succeed");
        assert!(err.contains("not supported"), "error should say the operation isn't supported: {err}");
    }

    #[tokio::test]
    async fn default_list_contacts_needing_enrichment_returns_an_empty_list() {
        let backend = MinimalBackend;
        let contacts = backend
            .list_contacts_needing_enrichment(50)
            .await
            .expect("the default must be Ok(empty), not an error — this one has no error path");
        assert!(contacts.is_empty());
    }

    #[tokio::test]
    async fn server_over_a_non_overriding_backend_still_answers_both_new_contact_tools() {
        let server = McpServer::new(MinimalBackend, "test-server", "test instructions");

        let update_result = server
            .update_contact_fields(Parameters(UpdateContactFieldsParams {
                contact_id: 1,
                fields: ContactFieldUpdates::default(),
                write_mode: None,
            }))
            .await;
        assert!(update_result.is_err(), "tool call should surface the backend's not-supported error");

        let list_result = server
            .list_contacts_needing_enrichment(Parameters(ListContactsNeedingEnrichmentParams { limit: None }))
            .await
            .expect("list tool should succeed with an empty list, not error");
        let ContentBlock::Text(text) = &list_result.content[0] else {
            panic!("expected a text content block, got {:?}", list_result.content[0]);
        };
        let parsed: serde_json::Value = serde_json::from_str(&text.text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "contacts": [] }));
    }

    #[tokio::test]
    async fn default_update_company_fields_returns_a_clear_not_supported_error() {
        let backend = MinimalBackend;
        let err = backend
            .update_company_fields(1, CompanyFieldUpdates::default(), false)
            .await
            .expect_err("a backend that doesn't override this must reject it, not silently succeed");
        assert!(err.contains("not supported"), "error should say the operation isn't supported: {err}");
    }

    #[tokio::test]
    async fn default_backup_vault_returns_a_clear_not_supported_error() {
        let backend = MinimalBackend;
        let err = backend
            .backup_vault("/tmp/whatever.db")
            .await
            .expect_err("a backend that doesn't override this must reject it, not silently succeed");
        assert!(err.contains("not supported"), "error should say the operation isn't supported: {err}");
    }

    #[tokio::test]
    async fn server_over_a_non_overriding_backend_still_rejects_backup_vault() {
        let server = McpServer::new(MinimalBackend, "test-server", "test instructions");
        let result =
            server.backup_vault(Parameters(BackupVaultParams { path: "/tmp/whatever.db".to_string() })).await;
        assert!(result.is_err(), "tool call should surface the backend's not-supported error");
    }

    #[tokio::test]
    async fn default_list_companies_needing_enrichment_returns_an_empty_list() {
        let backend = MinimalBackend;
        let companies = backend
            .list_companies_needing_enrichment(50)
            .await
            .expect("the default must be Ok(empty), not an error — this one has no error path");
        assert!(companies.is_empty());
    }

    /// Compiles a real `McpServer<MinimalBackend>` and calls both new tools
    /// through the exact same generated methods the `tools/call` JSON-RPC
    /// dispatcher uses — proof the non-breaking default plumbs all the way
    /// through the tool layer, not just the trait method in isolation.
    #[tokio::test]
    async fn server_over_a_non_overriding_backend_still_answers_both_new_tools() {
        let server = McpServer::new(MinimalBackend, "test-server", "test instructions");

        let update_result = server
            .update_company_fields(Parameters(UpdateCompanyFieldsParams {
                company_id: 1,
                fields: CompanyFieldUpdates::default(),
                overwrite: false,
            }))
            .await;
        assert!(update_result.is_err(), "tool call should surface the backend's not-supported error");

        let list_result = server
            .list_companies_needing_enrichment(Parameters(ListCompaniesNeedingEnrichmentParams { limit: None }))
            .await
            .expect("list tool should succeed with an empty list, not error");
        let ContentBlock::Text(text) = &list_result.content[0] else {
            panic!("expected a text content block, got {:?}", list_result.content[0]);
        };
        let parsed: serde_json::Value = serde_json::from_str(&text.text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "companies": [] }));
    }
}

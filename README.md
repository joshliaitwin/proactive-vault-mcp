# vault-mcp-core

A generic, reusable [MCP](https://modelcontextprotocol.io) (Model Context Protocol) server shell for local-first personal-data apps — contacts/company CRMs, or anything else shaped roughly like one.

This crate has zero knowledge of any particular app's schema or storage. It defines `McpBackend`, a trait describing six operations a contacts-style data source needs to support, and `McpServer`, a generic MCP server that exposes those operations as MCP tools over any transport [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) supports (stdio, HTTP/SSE, …).

Bring your own backend by implementing `McpBackend` against your own data. This crate handles the MCP protocol plumbing, tool schemas, and tool descriptions.

This is the extracted, generic core of [Proactive Vault](https://www.proactivepotential.com/vault)'s own MCP integration — Vault's app implements `McpBackend` against its real SQLite-backed contacts/company store and depends on this crate directly.

## The six tools

- `search_contacts` — free-text search with pagination
- `get_stats` — aggregate counts
- `get_company_network` — a company plus every contact at it
- `update_contact_field` — edit one field on a contact
- `merge_company_alias` — rename or merge a company
- `enqueue_enrichment` — flag contacts for manual follow-up

## Usage

```rust
use vault_mcp_core::{McpBackend, McpServer};

struct MyBackend { /* your own storage */ }

#[async_trait::async_trait]
impl McpBackend for MyBackend {
    type Contact = MyContact;   // your own type, just needs Serialize + JsonSchema
    type Company = MyCompany;
    type Stats = MyStats;
    type Error = MyError;       // needs Display

    async fn search_contacts(&self, query: &str, status: &str, limit: i64, offset: i64)
        -> Result<(i64, Vec<Self::Contact>), Self::Error> { /* ... */ }
    // ...the other five methods
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = McpServer::new(MyBackend { /* ... */ }, "my-app-mcp", "What this server does and how an agent should use it.");
    server.serve_stdio().await
}
```

For a transport other than stdio (HTTP/SSE, etc.), use `rmcp::ServiceExt::serve` directly with whichever `rmcp` transport you need — `McpServer` implements `rmcp::ServerHandler`, so it works with anything `rmcp` supports.

## Status

Early extraction from Proactive Vault's own implementation — the API may still shift as a second real consumer (Vault itself) gets fully wired up against it.

## License

MIT

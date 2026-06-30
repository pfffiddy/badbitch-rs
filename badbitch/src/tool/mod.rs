//! Tool framework: the `Tool` trait + `ToolRouter` + `ToolContext` + `input_schema`.
//!
//! The `Tool` trait, router, and `input_schema` are adapted from `learn-claude-code-rs`'s
//! `sfull/src/tool/mod.rs`. `ToolContext` is slimmed to what the OSINT tools need (config,
//! a shared HTTP client, the collect()/read_doc() docs scratch dir + sequence counter, and
//! the case-DB path) — none of sfull's agent-team / cron / worktree managers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde_json::Value;

use crate::config::Config;

pub mod corpus;
pub mod dossier;
pub mod entity;
pub mod geo;
pub mod infra;
pub mod links;
pub mod maltego;
pub mod people;
pub mod property;
pub mod shell;
pub mod web;

/// Wire description of a tool handed to the model (Ollama `tools[].function`).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Shared, cheaply-clonable state every tool call receives.
///
/// `reqwest::Client` is internally reference-counted, so cloning the context per call is cheap.
/// `doc_seq` mirrors badbitch's global `_doc_seq` (badbitch2.py:117) so `collect()` →
/// `read_doc()` ids line up across calls within a session.
#[derive(Clone)]
pub struct ToolContext {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub docs_dir: PathBuf,
    pub doc_seq: Arc<AtomicUsize>,
    pub db_path: PathBuf,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;

    async fn call(&self, context: ToolContext, input: Value) -> Result<String>;

    fn tool_spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: Some(self.description().to_string()),
            input_schema: self.input_schema(),
        }
    }
}

pub struct ToolRouter {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRouter {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn route<T>(mut self, tool: T) -> Self
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.name().to_string(), Box::new(tool));
        self
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.tool_spec()).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub async fn call(&self, context: &ToolContext, name: &str, input: Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))?;

        tool.call(context.clone(), input).await
    }
}

impl Default for ToolRouter {
    fn default() -> Self {
        Self::new()
    }
}

pub fn input_schema<T>() -> Value
where
    T: JsonSchema,
{
    serde_json::to_value(schemars::schema_for!(T)).expect("schema generation should not fail")
}

/// Full toolset: Phase-1 (16 tools) + Phase-2 infra/entity + geo Phase-2 + shell/link tools.
/// Mirrors badbitch2.py TOOLS + TOOLS.extend([...]) (lines 1367, 2787).
pub fn toolset() -> ToolRouter {
    ToolRouter::new()
        // ── Recon / corpus ──
        .route(web::ReconSweepTool)
        .route(web::WebSearchTool)
        .route(corpus::CollectTool)
        .route(corpus::QueryDocsTool)
        .route(corpus::ReadDocTool)
        // ── Web fetch ──
        .route(web::FetchRenderedTool)
        .route(links::FetchUrlTool)
        .route(links::FetchJsonTool)
        // ── People / social ──
        .route(people::PeopleSearchLinksTool)
        .route(people::SocialSearchLinksTool)
        .route(people::SherlockTool)
        .route(people::HoleheTool)
        .route(people::ExtractContactsTool)
        // ── Entity / breach ──
        .route(entity::TheharvesterTool)
        .route(entity::PhoneinfogaTool)
        .route(entity::DehashedTool)
        .route(entity::RocketreachTool)
        .route(entity::OpencorporatesTool)
        .route(entity::BreachCheckTool)
        // ── Geo ──
        .route(geo::GeocodeTool)
        .route(geo::ImageryLinksTool)
        .route(geo::SuncalcTool)
        // ── Property ──
        .route(property::FindCountyPortalsTool)
        .route(property::ArcgisQueryTool)
        .route(property::AttomPropertyTool)
        .route(property::RegridParcelTool)
        // ── Infra / domain ──
        .route(infra::ShodanTool)
        .route(infra::CensysTool)
        .route(infra::DnsdumpsterTool)
        .route(infra::VirustotalTool)
        .route(infra::IntelxTool)
        .route(infra::DnsReconTool)
        // ── Wayback / dossier ──
        .route(web::WaybackTool)
        .route(dossier::SaveDossierTool)
        .route(maltego::ExportToMaltegoTool)
        // ── Links ──
        .route(links::ReverseImageLinksTool)
        .route(links::CrimeDataLinksTool)
        .route(links::TorStatusTool)
        // ── Shell ──
        .route(shell::RunShellTool)
        .route(shell::PythonEvalTool)
        .route(shell::ExifMetadataTool)
}

//! Dossier finalize tool (badbitch2.py:266).

use badbitch_macros::tool;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::store;
use crate::tool::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveDossierInput {
    /// A parcel/APN, domain, person slug, or any stable key for the case.
    pub property_id: String,
    /// Human-readable address/label for the case.
    pub address: String,
    /// The full assembled Markdown dossier.
    pub dossier_markdown: String,
}

#[tool(
    name = "save_dossier",
    description = "Persist or update the full Markdown dossier for a case. property_id can be a parcel/APN, a domain, a person slug, or any stable key. Call once the report is assembled so the case is saved to the local SQLite store."
)]
pub async fn save_dossier(ctx: ToolContext, input: SaveDossierInput) -> String {
    store::save_dossier(
        &ctx.db_path,
        &input.property_id,
        &input.address,
        &input.dossier_markdown,
    )
}

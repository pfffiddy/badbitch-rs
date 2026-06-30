//! Neo4j export. Turns a saved case into a `<stem>.neo4j.cypher` script of idempotent
//! `MERGE` statements — load it into any Neo4j with `cypher-shell` (or paste into Browser)
//! for serious link analysis. Shares the entity/relationship model in `tool::graph`, so the
//! graph matches the Maltego/Graphviz export exactly; only the output format differs.
//!
//! Node labels: Person, Email, Phone, Domain, IPv4. Each node carries `value` (the merge key)
//! and `weight`. Relationship types are derived from the shared edge labels:
//!   email → HAS_EMAIL    phone → HAS_PHONE    domain → ASSOCIATED_WITH    ip → ASSOCIATED_WITH
//!   email-domain → EMAIL_DOMAIN              co-located → CO_LOCATED

use badbitch_macros::tool;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::store;
use crate::tool::ToolContext;
use crate::tool::graph::{Edge, Entity, build_edges, extract_entities};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportToNeo4jInput {
    /// The property_id / case key used in save_dossier().
    pub property_id: String,
    /// Optional output filename stem (defaults to a sanitized property_id).
    #[serde(default)]
    pub filename: String,
}

#[tool(
    name = "export_to_neo4j",
    description = "Export a saved case (by property_id) to a Neo4j Cypher script (<stem>.neo4j.cypher) of idempotent MERGE statements for nodes (Person/Email/Phone/Domain/IPv4) and typed relationships (HAS_EMAIL, HAS_PHONE, ASSOCIATED_WITH, EMAIL_DOMAIN, CO_LOCATED). Load it with `cypher-shell -f <file>` or paste into Neo4j Browser. Same entity/relationship model as export_to_maltego. Run save_dossier first."
)]
pub async fn export_to_neo4j(ctx: ToolContext, input: ExportToNeo4jInput) -> String {
    let (_address, dossier_md, _updated) = match store::load_raw(&ctx.db_path, &input.property_id) {
        Ok(Some(r)) => r,
        Ok(None) => return format!("[not found] no saved case '{}'", input.property_id),
        Err(e) => return format!("[load error] {e}"),
    };

    let entities = extract_entities(&dossier_md);
    if entities.is_empty() {
        return format!(
            "[nothing to export] no entities found in case '{}'",
            input.property_id
        );
    }
    let edges = build_edges(&entities, &dossier_md);

    let stem = sanitize(if input.filename.trim().is_empty() {
        &input.property_id
    } else {
        &input.filename
    });
    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cypher_path = workdir.join(&stem).with_extension("neo4j.cypher");

    let script = render_cypher(&input.property_id, &entities, &edges);
    if let Err(e) = std::fs::write(&cypher_path, &script) {
        return format!("[neo4j error] could not write {}: {e}", cypher_path.display());
    }

    format!(
        "[success] Neo4j export for '{}'\n\
         nodes: {}  relationships: {}\n\
         cypher: {}\n\n\
         Load it:  cypher-shell -u neo4j -p <password> -f {}\n\
         or paste the file's contents into Neo4j Browser.",
        input.property_id,
        entities.len(),
        edges.len(),
        cypher_path.display(),
        cypher_path.display(),
    )
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

/// Node label for an entity type (the Maltego type names map to clean Neo4j labels).
fn node_label(ty: &str) -> &'static str {
    match ty {
        "Person" => "Person",
        "Email" => "Email",
        "Phone" => "Phone",
        "Domain" => "Domain",
        "IPv4Address" => "IPv4",
        _ => "Entity",
    }
}

/// Relationship type for a shared edge label.
fn rel_type(label: &str) -> &'static str {
    match label {
        "email" => "HAS_EMAIL",
        "phone" => "HAS_PHONE",
        "email-domain" => "EMAIL_DOMAIN",
        "co-located" => "CO_LOCATED",
        // "domain" / "ip" subject edges, and any future labels:
        _ => "ASSOCIATED_WITH",
    }
}

/// Escape a string for a single-quoted Cypher literal.
fn cy(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn render_cypher(case_id: &str, entities: &[Entity], edges: &[Edge]) -> String {
    let mut s = String::new();
    s.push_str(&format!("// badbitch-rs export — case '{}'\n", cy(case_id)));
    s.push_str("// Idempotent: MERGE keys on `value`, so re-running updates rather than duplicates.\n\n");

    s.push_str("// ── Nodes ──\n");
    for e in entities {
        s.push_str(&format!(
            "MERGE (n:{label} {{value: '{val}'}}) SET n.weight = {w}, n.case = '{case}';\n",
            label = node_label(&e.ty),
            val = cy(&e.value),
            w = e.weight,
            case = cy(case_id),
        ));
    }

    s.push_str("\n// ── Relationships ──\n");
    for edge in edges {
        s.push_str(&format!(
            "MATCH (a:{sl} {{value: '{sv}'}}), (b:{tl} {{value: '{tv}'}}) \
             MERGE (a)-[:{rel}]->(b);\n",
            sl = node_label(&edge.src.ty),
            sv = cy(&edge.src.value),
            tl = node_label(&edge.tgt.ty),
            tv = cy(&edge.tgt.value),
            rel = rel_type(&edge.label),
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::graph::SAMPLE_MD;

    #[test]
    fn rel_types_map() {
        assert_eq!(rel_type("email"), "HAS_EMAIL");
        assert_eq!(rel_type("phone"), "HAS_PHONE");
        assert_eq!(rel_type("email-domain"), "EMAIL_DOMAIN");
        assert_eq!(rel_type("co-located"), "CO_LOCATED");
        assert_eq!(rel_type("domain"), "ASSOCIATED_WITH");
    }

    #[test]
    fn cypher_escapes_quotes() {
        assert_eq!(cy("O'Brien"), "O\\'Brien");
    }

    #[test]
    fn renders_nodes_and_relationships() {
        let ents = extract_entities(SAMPLE_MD);
        let edges = build_edges(&ents, SAMPLE_MD);
        let script = render_cypher("test-case", &ents, &edges);

        // A node MERGE for the subject and the email.
        assert!(script.contains("MERGE (n:Person {value: 'Jane Q Public'})"));
        assert!(script.contains("MERGE (n:Email {value: 'jane.public@example.com'})"));
        // IPv4 label remap.
        assert!(script.contains("MERGE (n:IPv4 {value: '203.0.113.42'})"));
        // A typed relationship: email → its domain.
        assert!(script.contains(":EMAIL_DOMAIN]->"));
        // Co-location relationship.
        assert!(script.contains(":CO_LOCATED]->"));
    }
}

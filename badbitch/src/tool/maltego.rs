//! Maltego + Graphviz export. Level-2 integration: run the investigation, save a dossier,
//! then `export_to_maltego` turns the saved case into importable artifacts:
//!   - `<stem>.maltego.entities.csv` — Type,Value,Weight,Note (Maltego "Import from CSV")
//!   - `<stem>.maltego.links.csv`    — typed Source→Target relationships with labels
//!   - `<stem>.graphviz.dot`         — a labelled relationship graph (when graphviz=true)
//!   - `<stem>.graphviz.png`         — rendered if the `dot` CLI is on PATH (else skipped)
//!
//! The entity + relationship model lives in `tool::graph` (shared with the Neo4j exporter).

use std::path::Path;

use badbitch_macros::tool;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::shell;
use crate::store;
use crate::tool::ToolContext;
use crate::tool::graph::{Edge, Entity, build_edges, extract_entities};

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportToMaltegoInput {
    /// The property_id / case key used in save_dossier().
    pub property_id: String,
    /// Optional output filename stem (defaults to a sanitized property_id).
    #[serde(default)]
    pub filename: String,
    /// Also emit a Graphviz .dot graph (and render .png if the `dot` CLI is installed).
    /// Default true; set false to write only the Maltego CSVs.
    #[serde(default = "default_true")]
    pub graphviz: bool,
}

#[tool(
    name = "export_to_maltego",
    description = "Export a saved case (by property_id) to Maltego-importable CSV (entities + typed links) and, unless graphviz=false, a Graphviz .dot graph (rendered to .png if the `dot` CLI is installed). Extracts persons, emails, phones, domains, IPs and links them via the shared relationship model (subject anchor, email→domain, domain↔IP co-location). Run save_dossier first. In Maltego: Import → Table/CSV → the .entities.csv file."
)]
pub async fn export_to_maltego(ctx: ToolContext, input: ExportToMaltegoInput) -> String {
    let (_address, dossier_md, _updated) = match store::load_raw(&ctx.db_path, &input.property_id) {
        Ok(Some(r)) => r,
        Ok(None) => return format!("[not found] no saved case '{}'", input.property_id),
        Err(e) => return format!("[load error] {e}"),
    };

    let stem = sanitize(if input.filename.trim().is_empty() {
        &input.property_id
    } else {
        &input.filename
    });
    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let base = workdir.join(&stem);
    let entities_csv = base.with_extension("maltego.entities.csv");
    let links_csv = base.with_extension("maltego.links.csv");
    let dot_file = base.with_extension("graphviz.dot");
    let png_file = base.with_extension("graphviz.png");

    let entities = extract_entities(&dossier_md);
    if entities.is_empty() {
        return format!(
            "[nothing to export] no entities (emails/phones/domains/IPs/subject) found in case '{}'",
            input.property_id
        );
    }
    let edges = build_edges(&entities, &dossier_md);

    if let Err(e) = write_maltego_csvs(&entities, &edges, &entities_csv, &links_csv) {
        return format!("[csv error] {e}");
    }

    let mut out = format!(
        "[success] Maltego export for '{}'\n\
         entities: {} -> {}\n\
         links:    {} -> {}\n",
        input.property_id,
        entities.len(),
        entities_csv.display(),
        edges.len(),
        links_csv.display(),
    );

    // Graphviz is fully optional and degrades cleanly at each step.
    if input.graphviz {
        match write_dot(&entities, &edges, &dot_file) {
            Ok(()) => {
                out.push_str(&format!("graphviz: {}\n", dot_file.display()));
                if shell::have("dot").await {
                    match shell::run(
                        "dot",
                        &["-Tpng", &dot_file.to_string_lossy(), "-o", &png_file.to_string_lossy()],
                        60,
                    )
                    .await
                    {
                        Ok(o) if !o.timed_out && png_file.exists() => {
                            out.push_str(&format!("rendered: {}\n", png_file.display()));
                        }
                        Ok(o) if o.timed_out => {
                            out.push_str("[note] PNG render timed out; the .dot is still valid (`dot -Tpng <file>`).\n");
                        }
                        _ => {
                            out.push_str("[note] `dot` failed to render PNG; the .dot is still valid.\n");
                        }
                    }
                } else {
                    out.push_str("[note] PNG skipped — graphviz `dot` not on PATH (apt install graphviz).\n");
                }
            }
            Err(e) => out.push_str(&format!("[note] could not write .dot: {e}\n")),
        }
    }

    out.push_str("\nMaltego: Import → Table/CSV → the .entities.csv (map column 1 to Entity Type).");
    out
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

fn csv_escape(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn write_maltego_csvs(
    entities: &[Entity],
    edges: &[Edge],
    epath: &Path,
    lpath: &Path,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut ef = std::fs::File::create(epath)?;
    writeln!(ef, "Type,Value,Weight,Note")?;
    for e in entities {
        writeln!(ef, "{},{},{},", e.ty, csv_escape(&e.value), e.weight)?;
    }

    let mut lf = std::fs::File::create(lpath)?;
    writeln!(lf, "SourceType,SourceValue,TargetType,TargetValue,LinkLabel")?;
    for edge in edges {
        writeln!(
            lf,
            "{},{},{},{},{}",
            edge.src.ty,
            csv_escape(&edge.src.value),
            edge.tgt.ty,
            csv_escape(&edge.tgt.value),
            csv_escape(&edge.label)
        )?;
    }
    Ok(())
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn write_dot(entities: &[Entity], edges: &[Edge], path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "digraph OSINT {{")?;
    writeln!(f, "  rankdir=LR;")?;
    writeln!(f, "  node [fontname=\"Helvetica\"];")?;
    writeln!(f, "  edge [fontname=\"Helvetica\", fontsize=10];")?;
    for e in entities {
        let shape = match e.ty.as_str() {
            "Person" => "ellipse",
            "Email" | "Phone" => "box",
            "Domain" => "component",
            "IPv4Address" => "cylinder",
            _ => "box",
        };
        writeln!(
            f,
            "  \"{}\" [label=\"{}\\n{}\", shape={}];",
            dot_escape(&e.value),
            dot_escape(&e.ty),
            dot_escape(&e.value),
            shape
        )?;
    }
    for edge in edges {
        writeln!(
            f,
            "  \"{}\" -> \"{}\" [label=\"{}\"];",
            dot_escape(&edge.src.value),
            dot_escape(&edge.tgt.value),
            dot_escape(&edge.label)
        )?;
    }
    writeln!(f, "}}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::graph::SAMPLE_MD;

    #[test]
    fn csv_escaping_quotes() {
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn writes_csv_and_dot_files() {
        let ents = extract_entities(SAMPLE_MD);
        let edges = build_edges(&ents, SAMPLE_MD);
        let dir = std::env::temp_dir().join(format!("bb_maltego_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ecsv = dir.join("e.csv");
        let lcsv = dir.join("l.csv");
        let dot = dir.join("g.dot");

        write_maltego_csvs(&ents, &edges, &ecsv, &lcsv).unwrap();
        write_dot(&ents, &edges, &dot).unwrap();

        let e = std::fs::read_to_string(&ecsv).unwrap();
        assert!(e.starts_with("Type,Value,Weight,Note"));
        assert!(e.contains("Email,\"jane.public@example.com\",80,"));

        let l = std::fs::read_to_string(&lcsv).unwrap();
        assert!(l.contains("email-domain"));

        let g = std::fs::read_to_string(&dot).unwrap();
        assert!(g.trim_start().starts_with("digraph OSINT"));
        assert!(g.contains("[label=\"email-domain\"]"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

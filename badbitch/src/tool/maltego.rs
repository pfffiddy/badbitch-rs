//! Maltego + Graphviz export. Level-2 integration: run the investigation, save a dossier,
//! then `export_to_maltego` turns the saved case into importable artifacts:
//!   - `<stem>.maltego.entities.csv` — Type,Value,Weight,Note (Maltego "Import from CSV")
//!   - `<stem>.maltego.links.csv`    — Source/Target rows linking the subject to each entity
//!   - `<stem>.graphviz.dot`         — a relationship graph
//!   - `<stem>.graphviz.png`         — rendered if the `dot` CLI is on PATH (else skipped)
//!
//! Entities are extracted from the dossier Markdown with self-contained regexes (emails,
//! phones, domains, IPs) plus a best-effort primary-subject line. Kept deliberately simple
//! and dependency-free so it compiles without exposing the private tool regexes.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

use badbitch_macros::tool;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::shell;
use crate::store;
use crate::tool::ToolContext;

static RE_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap());
static RE_PHONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}").unwrap());
// Domains/IPs scanned in-context (not anchored), excluding emails handled separately.
static RE_DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,24}\b").unwrap()
});
static RE_IP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap());

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportToMaltegoInput {
    /// The property_id / case key used in save_dossier().
    pub property_id: String,
    /// Optional output filename stem (defaults to a sanitized property_id).
    #[serde(default)]
    pub filename: String,
}

#[tool(
    name = "export_to_maltego",
    description = "Export a saved case (by property_id) to Maltego-importable CSV (entities + links) AND a Graphviz .dot graph (rendered to .png if the `dot` CLI is installed). Extracts persons, emails, phones, domains, and IPs from the dossier and links them to the primary subject. Run save_dossier first. In Maltego: Import → Table/CSV → the .entities.csv file."
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
    let links = build_links(&entities);

    if let Err(e) = write_maltego_csvs(&entities, &links, &entities_csv, &links_csv) {
        return format!("[csv error] {e}");
    }
    let dot_written = write_dot(&entities, &links, &dot_file).is_ok();

    // Render PNG only if graphviz `dot` is available.
    let mut png_ok = false;
    if dot_written
        && shell::have("dot").await
        && let Ok(o) = shell::run(
            "dot",
            &["-Tpng", &dot_file.to_string_lossy(), "-o", &png_file.to_string_lossy()],
            60,
        )
        .await
    {
        png_ok = !o.timed_out && png_file.exists();
    }

    let mut out = format!(
        "[success] Maltego/Graphviz export for '{}'\n\
         entities: {} -> {}\n\
         links:    {} -> {}\n",
        input.property_id,
        entities.len(),
        entities_csv.display(),
        links.len(),
        links_csv.display(),
    );
    if dot_written {
        out.push_str(&format!("graphviz: {}\n", dot_file.display()));
    }
    if png_ok {
        out.push_str(&format!("rendered: {}\n", png_file.display()));
    } else {
        out.push_str("[note] PNG skipped (install graphviz `dot` to render the .dot file)\n");
    }
    out.push_str("\nMaltego: Import → Table/CSV → the .entities.csv (map column 1 to Entity Type).");
    out
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Entity {
    ty: String,
    value: String,
    weight: u32,
}

/// Extract typed entities from the dossier Markdown. Dedupes per (type,value).
fn extract_entities(md: &str) -> Vec<Entity> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<Entity> = Vec::new();
    let push = |ty: &str, value: &str, weight: u32, out: &mut Vec<Entity>, seen: &mut BTreeSet<(String, String)>| {
        let v = value.trim();
        if v.is_empty() {
            return;
        }
        if seen.insert((ty.to_string(), v.to_string())) {
            out.push(Entity { ty: ty.to_string(), value: v.to_string(), weight });
        }
    };

    // Primary subject: first line that looks like "subject: X" / "name: X".
    if let Some(val) = md
        .lines()
        .find(|l| {
            let low = l.to_lowercase();
            low.contains("subject") || low.contains("name:")
        })
        .and_then(|l| l.split(':').nth(1))
    {
        push("Person", val, 100, &mut out, &mut seen);
    }

    for m in RE_EMAIL.find_iter(md) {
        push("Email", m.as_str(), 80, &mut out, &mut seen);
    }
    for m in RE_PHONE.find_iter(md) {
        push("Phone", m.as_str(), 70, &mut out, &mut seen);
    }
    for m in RE_IP.find_iter(md) {
        push("IPv4Address", m.as_str(), 60, &mut out, &mut seen);
    }
    // Domains: skip anything already captured as part of an email local@domain.
    let email_domains: BTreeSet<String> = RE_EMAIL
        .find_iter(md)
        .filter_map(|m| m.as_str().rsplit('@').next().map(|d| d.to_lowercase()))
        .collect();
    for m in RE_DOMAIN.find_iter(md) {
        let d = m.as_str();
        if !email_domains.contains(&d.to_lowercase()) {
            push("Domain", d, 75, &mut out, &mut seen);
        }
    }
    out
}

/// Link the primary subject (first Person) to every other entity. Simple star topology —
/// the most useful default for a single-subject OSINT case.
fn build_links(entities: &[Entity]) -> Vec<(Entity, Entity)> {
    let subject = entities.iter().find(|e| e.ty == "Person").cloned();
    let Some(subject) = subject else {
        return Vec::new();
    };
    entities
        .iter()
        .filter(|e| **e != subject)
        .map(|e| (subject.clone(), e.clone()))
        .collect()
}

fn csv_escape(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn write_maltego_csvs(
    entities: &[Entity],
    links: &[(Entity, Entity)],
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
    for (src, tgt) in links {
        writeln!(
            lf,
            "{},{},{},{},{}",
            src.ty,
            csv_escape(&src.value),
            tgt.ty,
            csv_escape(&tgt.value),
            csv_escape("related")
        )?;
    }
    Ok(())
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn write_dot(entities: &[Entity], links: &[(Entity, Entity)], path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "digraph OSINT {{")?;
    writeln!(f, "  rankdir=LR;")?;
    writeln!(f, "  node [fontname=\"Helvetica\"];")?;
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
    for (src, tgt) in links {
        writeln!(
            f,
            "  \"{}\" -> \"{}\";",
            dot_escape(&src.value),
            dot_escape(&tgt.value)
        )?;
    }
    writeln!(f, "}}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD: &str = "# Dossier\n\
        Subject: Jane Q Public\n\
        Primary email: jane.public@example.com\n\
        Alt contact: 432-555-0100 and +1 (325) 555-0199\n\
        Owner domain: acme-holdings.com\n\
        Hosting IP: 203.0.113.42\n\
        Registered at example.org via jane.public@example.com\n";

    #[test]
    fn extracts_typed_entities_and_dedupes() {
        let ents = extract_entities(MD);
        let by_type = |t: &str| ents.iter().filter(|e| e.ty == t).count();

        // Primary subject captured once.
        assert_eq!(by_type("Person"), 1);
        assert!(ents.iter().any(|e| e.ty == "Person" && e.value == "Jane Q Public"));

        // Email captured once despite appearing twice in the text.
        assert_eq!(by_type("Email"), 1);
        assert!(ents.iter().any(|e| e.value == "jane.public@example.com"));

        // Both phones present.
        assert_eq!(by_type("Phone"), 2);

        // IPv4 captured.
        assert!(ents.iter().any(|e| e.ty == "IPv4Address" && e.value == "203.0.113.42"));

        // example.com is an email domain -> excluded; acme-holdings.com & example.org kept.
        let domains: Vec<&str> = ents.iter().filter(|e| e.ty == "Domain").map(|e| e.value.as_str()).collect();
        assert!(domains.contains(&"acme-holdings.com"));
        assert!(domains.contains(&"example.org"));
        assert!(!domains.contains(&"example.com"));
    }

    #[test]
    fn links_star_from_subject() {
        let ents = extract_entities(MD);
        let links = build_links(&ents);
        // Every link sources from the Person, and there's one per non-subject entity.
        assert_eq!(links.len(), ents.len() - 1);
        assert!(links.iter().all(|(s, _)| s.ty == "Person"));
    }

    #[test]
    fn csv_escaping_quotes() {
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn writes_csv_and_dot_files() {
        let ents = extract_entities(MD);
        let links = build_links(&ents);
        let dir = std::env::temp_dir().join(format!("bb_maltego_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ecsv = dir.join("e.csv");
        let lcsv = dir.join("l.csv");
        let dot = dir.join("g.dot");

        write_maltego_csvs(&ents, &links, &ecsv, &lcsv).unwrap();
        write_dot(&ents, &links, &dot).unwrap();

        let e = std::fs::read_to_string(&ecsv).unwrap();
        assert!(e.starts_with("Type,Value,Weight,Note"));
        assert!(e.contains("Email,\"jane.public@example.com\",80,"));

        let g = std::fs::read_to_string(&dot).unwrap();
        assert!(g.trim_start().starts_with("digraph OSINT"));
        assert!(g.contains("->"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

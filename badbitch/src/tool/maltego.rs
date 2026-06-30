//! Maltego + Graphviz export. Level-2 integration: run the investigation, save a dossier,
//! then `export_to_maltego` turns the saved case into importable artifacts:
//!   - `<stem>.maltego.entities.csv` — Type,Value,Weight,Note (Maltego "Import from CSV")
//!   - `<stem>.maltego.links.csv`    — typed Source→Target relationships with labels
//!   - `<stem>.graphviz.dot`         — a labelled relationship graph
//!   - `<stem>.graphviz.png`         — rendered if the `dot` CLI is on PATH (else skipped)
//!
//! Entities are extracted from the dossier Markdown with self-contained regexes (emails,
//! phones, domains, IPs) plus a best-effort primary-subject line.
//!
//! Relationship model (richer than a plain star):
//!   1. SUBJECT ANCHOR — the primary Person links to every other entity, with a typed label
//!      (email / phone / domain / ip).
//!   2. DERIVED email→domain — each Email links to the Domain in its `@host` part (a fact,
//!      not a guess), so credentials cluster under their domain.
//!   3. CO-LOCATION domain↔IP — a Domain and an IPv4 mentioned on the SAME source line are
//!      linked ("co-located"), capturing "acme.com resolves to 203.0.113.42"-style lines.
//!
//! Email local-parts (e.g. `jane.public` in `jane.public@…`) are masked before domain
//! scanning so they never leak in as spurious Domain nodes; the email's real host is added
//! as a Domain node explicitly so the email→domain edge always has both endpoints.

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
// Domains/IPs scanned in-context (not anchored), over an email-masked copy of the text.
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
    description = "Export a saved case (by property_id) to Maltego-importable CSV (entities + typed links) AND a Graphviz .dot graph (rendered to .png if the `dot` CLI is installed). Extracts persons, emails, phones, domains, and IPs from the dossier and links them with a relationship model: subject anchor, derived email→domain edges, and domain↔IP co-location. Run save_dossier first. In Maltego: Import → Table/CSV → the .entities.csv file."
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
    let dot_written = write_dot(&entities, &edges, &dot_file).is_ok();

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
        edges.len(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edge {
    src: Entity,
    tgt: Entity,
    label: String,
}

/// Replace every email match with spaces so domain/phone scanning can't pick up an email's
/// local-part (`jane.public`) or its host as a free-floating token. The host is re-added as a
/// Domain explicitly in `extract_entities`, keeping the email→domain edge well-formed.
fn mask_emails(md: &str) -> String {
    RE_EMAIL
        .replace_all(md, |c: &regex::Captures| " ".repeat(c[0].len()))
        .into_owned()
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

    // Emails first; capture their hosts so we can both add them as Domain nodes and draw edges.
    let mut email_hosts: BTreeSet<String> = BTreeSet::new();
    for m in RE_EMAIL.find_iter(md) {
        push("Email", m.as_str(), 80, &mut out, &mut seen);
        if let Some(host) = m.as_str().rsplit('@').next() {
            email_hosts.insert(host.to_lowercase());
        }
    }

    // Phones and IPs over the raw text (they don't collide with email contexts).
    for m in RE_PHONE.find_iter(md) {
        push("Phone", m.as_str(), 70, &mut out, &mut seen);
    }
    for m in RE_IP.find_iter(md) {
        push("IPv4Address", m.as_str(), 60, &mut out, &mut seen);
    }

    // Domains over an email-masked copy (no local-part / host leakage)…
    let masked = mask_emails(md);
    for m in RE_DOMAIN.find_iter(&masked) {
        push("Domain", m.as_str(), 75, &mut out, &mut seen);
    }
    // …then add each email's host as a Domain node so email→domain edges have an endpoint.
    for host in email_hosts {
        push("Domain", &host, 75, &mut out, &mut seen);
    }
    out
}

fn label_for(ty: &str) -> &'static str {
    match ty {
        "Email" => "email",
        "Phone" => "phone",
        "Domain" => "domain",
        "IPv4Address" => "ip",
        _ => "linked",
    }
}

/// Build the relationship graph: subject star (typed), derived email→domain, and per-line
/// domain↔IP co-location. Edges are deduped by (src.value, tgt.value, label).
fn build_edges(entities: &[Entity], md: &str) -> Vec<Edge> {
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let add = |src: &Entity, tgt: &Entity, label: &str, edges: &mut Vec<Edge>, seen: &mut BTreeSet<(String, String, String)>| {
        if src.value == tgt.value {
            return;
        }
        let key = (src.value.clone(), tgt.value.clone(), label.to_string());
        if seen.insert(key) {
            edges.push(Edge { src: src.clone(), tgt: tgt.clone(), label: label.to_string() });
        }
    };

    let find = |ty: &str, value: &str| -> Option<&Entity> {
        entities.iter().find(|e| e.ty == ty && e.value.eq_ignore_ascii_case(value))
    };

    // 1. Subject anchor.
    if let Some(subject) = entities.iter().find(|e| e.ty == "Person") {
        for e in entities.iter().filter(|e| **e != *subject) {
            add(subject, e, label_for(&e.ty), &mut edges, &mut seen);
        }
    }

    // 2. Derived email → domain (the email's own host).
    for e in entities.iter().filter(|e| e.ty == "Email") {
        if let Some(host) = e.value.rsplit('@').next()
            && let Some(dom) = find("Domain", host)
        {
            add(e, dom, "email-domain", &mut edges, &mut seen);
        }
    }

    // 3. Co-location: a Domain and an IPv4 on the same source line are linked.
    let domains: Vec<&Entity> = entities.iter().filter(|e| e.ty == "Domain").collect();
    let ips: Vec<&Entity> = entities.iter().filter(|e| e.ty == "IPv4Address").collect();
    if !domains.is_empty() && !ips.is_empty() {
        for line in md.lines() {
            let on_line: Vec<&&Entity> = domains.iter().filter(|d| line.contains(&d.value)).collect();
            if on_line.is_empty() {
                continue;
            }
            for ip in ips.iter().filter(|ip| line.contains(&ip.value)) {
                for d in &on_line {
                    add(d, ip, "co-located", &mut edges, &mut seen);
                }
            }
        }
    }

    edges
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

    const MD: &str = "# Dossier\n\
        Subject: Jane Q Public\n\
        Primary email: jane.public@example.com\n\
        Alt contact: 432-555-0100 and +1 (325) 555-0199\n\
        Owner domain: acme-holdings.com\n\
        Hosting: acme-holdings.com resolves to 203.0.113.42 (A record)\n\
        Registered at example.org via jane.public@example.com\n";

    #[test]
    fn extracts_typed_entities_and_dedupes() {
        let ents = extract_entities(MD);
        let by_type = |t: &str| ents.iter().filter(|e| e.ty == t).count();

        assert_eq!(by_type("Person"), 1);
        assert!(ents.iter().any(|e| e.ty == "Person" && e.value == "Jane Q Public"));

        // Email captured once despite appearing twice.
        assert_eq!(by_type("Email"), 1);
        assert_eq!(by_type("Phone"), 2);
        assert!(ents.iter().any(|e| e.ty == "IPv4Address" && e.value == "203.0.113.42"));

        let domains: Vec<&str> = ents.iter().filter(|e| e.ty == "Domain").map(|e| e.value.as_str()).collect();
        assert!(domains.contains(&"acme-holdings.com"));
        assert!(domains.contains(&"example.org"));
        // example.com is added as a node via the email host (for the email→domain edge)…
        assert!(domains.contains(&"example.com"));
        // …but the email LOCAL-part must never leak in as a domain.
        assert!(!domains.contains(&"jane.public"));
    }

    #[test]
    fn relationship_model_has_typed_edges() {
        let ents = extract_entities(MD);
        let edges = build_edges(&ents, MD);

        // Subject anchors to every other entity, with typed labels.
        let subject_edges: Vec<&Edge> = edges.iter().filter(|e| e.src.ty == "Person").collect();
        assert_eq!(subject_edges.len(), ents.len() - 1);
        assert!(subject_edges.iter().any(|e| e.tgt.ty == "Email" && e.label == "email"));
        assert!(subject_edges.iter().any(|e| e.tgt.ty == "Phone" && e.label == "phone"));

        // Derived email → domain edge to the email's own host.
        assert!(edges.iter().any(|e| {
            e.label == "email-domain"
                && e.src.value == "jane.public@example.com"
                && e.tgt.value == "example.com"
        }));

        // Co-location: acme-holdings.com and 203.0.113.42 share a line.
        assert!(edges.iter().any(|e| {
            e.label == "co-located"
                && e.src.value == "acme-holdings.com"
                && e.tgt.value == "203.0.113.42"
        }));
    }

    #[test]
    fn csv_escaping_quotes() {
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn writes_csv_and_dot_files() {
        let ents = extract_entities(MD);
        let edges = build_edges(&ents, MD);
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

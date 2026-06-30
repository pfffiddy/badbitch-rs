//! Shared entity + relationship model for the graph exporters (`export_to_maltego`,
//! `export_to_neo4j`). One extractor, one relationship model — the exporters differ only in
//! output format, so they must agree on the graph itself.
//!
//! Entities are pulled from dossier Markdown with self-contained regexes (emails, phones,
//! domains, IPs) plus a best-effort primary-subject line.
//!
//! Relationship model (richer than a plain star):
//!   1. SUBJECT ANCHOR — the primary Person links to every other entity, typed label
//!      (email / phone / domain / ip).
//!   2. DERIVED email→domain — each Email links to the Domain in its `@host` part (a fact).
//!   3. CO-LOCATION domain↔IP — a Domain and an IPv4 on the SAME source line are linked.
//!
//! Email local-parts (e.g. `jane.public`) are masked before domain scanning so they never
//! leak in as spurious Domain nodes; the email's real host is added as a Domain node so the
//! email→domain edge always has both endpoints.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

static RE_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap());
static RE_PHONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}").unwrap());
static RE_DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,24}\b").unwrap()
});
static RE_IP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap());

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entity {
    pub ty: String,
    pub value: String,
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub src: Entity,
    pub tgt: Entity,
    pub label: String,
}

/// Replace every email match with spaces so domain/phone scanning can't pick up an email's
/// local-part (`jane.public`) or host as a free-floating token.
fn mask_emails(md: &str) -> String {
    RE_EMAIL
        .replace_all(md, |c: &regex::Captures| " ".repeat(c[0].len()))
        .into_owned()
}

/// Extract typed entities from dossier Markdown. Dedupes per (type, value).
pub fn extract_entities(md: &str) -> Vec<Entity> {
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

    let mut email_hosts: BTreeSet<String> = BTreeSet::new();
    for m in RE_EMAIL.find_iter(md) {
        push("Email", m.as_str(), 80, &mut out, &mut seen);
        if let Some(host) = m.as_str().rsplit('@').next() {
            email_hosts.insert(host.to_lowercase());
        }
    }

    for m in RE_PHONE.find_iter(md) {
        push("Phone", m.as_str(), 70, &mut out, &mut seen);
    }
    for m in RE_IP.find_iter(md) {
        push("IPv4Address", m.as_str(), 60, &mut out, &mut seen);
    }

    let masked = mask_emails(md);
    for m in RE_DOMAIN.find_iter(&masked) {
        push("Domain", m.as_str(), 75, &mut out, &mut seen);
    }
    for host in email_hosts {
        push("Domain", &host, 75, &mut out, &mut seen);
    }
    out
}

/// Human label for a subject→entity edge.
pub fn label_for(ty: &str) -> &'static str {
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
pub fn build_edges(entities: &[Entity], md: &str) -> Vec<Edge> {
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

#[cfg(test)]
pub(crate) const SAMPLE_MD: &str = "# Dossier\n\
    Subject: Jane Q Public\n\
    Primary email: jane.public@example.com\n\
    Alt contact: 432-555-0100 and +1 (325) 555-0199\n\
    Owner domain: acme-holdings.com\n\
    Hosting: acme-holdings.com resolves to 203.0.113.42 (A record)\n\
    Registered at example.org via jane.public@example.com\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_typed_entities_and_dedupes() {
        let ents = extract_entities(SAMPLE_MD);
        let by_type = |t: &str| ents.iter().filter(|e| e.ty == t).count();

        assert_eq!(by_type("Person"), 1);
        assert!(ents.iter().any(|e| e.ty == "Person" && e.value == "Jane Q Public"));
        assert_eq!(by_type("Email"), 1);
        assert_eq!(by_type("Phone"), 2);
        assert!(ents.iter().any(|e| e.ty == "IPv4Address" && e.value == "203.0.113.42"));

        let domains: Vec<&str> = ents.iter().filter(|e| e.ty == "Domain").map(|e| e.value.as_str()).collect();
        assert!(domains.contains(&"acme-holdings.com"));
        assert!(domains.contains(&"example.org"));
        assert!(domains.contains(&"example.com"));
        assert!(!domains.contains(&"jane.public"));
    }

    #[test]
    fn relationship_model_has_typed_edges() {
        let ents = extract_entities(SAMPLE_MD);
        let edges = build_edges(&ents, SAMPLE_MD);

        let subject_edges: Vec<&Edge> = edges.iter().filter(|e| e.src.ty == "Person").collect();
        assert_eq!(subject_edges.len(), ents.len() - 1);
        assert!(subject_edges.iter().any(|e| e.tgt.ty == "Email" && e.label == "email"));
        assert!(subject_edges.iter().any(|e| e.tgt.ty == "Phone" && e.label == "phone"));

        assert!(edges.iter().any(|e| {
            e.label == "email-domain" && e.src.value == "jane.public@example.com" && e.tgt.value == "example.com"
        }));
        assert!(edges.iter().any(|e| {
            e.label == "co-located" && e.src.value == "acme-holdings.com" && e.tgt.value == "203.0.113.42"
        }));
    }
}

"""Shared entity/relationship extraction for the badbitch-rs Maltego transforms.

This is a faithful Python port of the Rust exporter in
`badbitch/src/tool/maltego.rs` so a Maltego graph built via local transforms
matches what `export_to_maltego` writes to CSV. Keep the two in sync.

No third-party deps — only the stdlib (re, os, sqlite3).
"""

import os
import re
import sqlite3
from typing import Dict, List, Optional, Tuple

# ── Regexes (mirror maltego.rs) ──────────────────────────────────────────────
RE_EMAIL = re.compile(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}")
RE_PHONE = re.compile(r"(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}")
RE_DOMAIN = re.compile(
    r"\b(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,24}\b"
)
RE_IP = re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b")

# Maltego entity type for each internal kind.
MALTEGO_TYPE = {
    "Person": "maltego.Person",
    "Email": "maltego.EmailAddress",
    "Phone": "maltego.PhoneNumber",
    "Domain": "maltego.Domain",
    "IPv4Address": "maltego.IPv4Address",
}

WEIGHT = {"Person": 100, "Email": 80, "Domain": 75, "Phone": 70, "IPv4Address": 60}


class Entity:
    __slots__ = ("ty", "value", "weight")

    def __init__(self, ty: str, value: str):
        self.ty = ty
        self.value = value
        self.weight = WEIGHT.get(ty, 50)

    def __eq__(self, other):
        return isinstance(other, Entity) and (self.ty, self.value) == (other.ty, other.value)

    def __hash__(self):
        return hash((self.ty, self.value))

    def __repr__(self):
        return f"Entity({self.ty}, {self.value!r})"


class Edge:
    __slots__ = ("src", "tgt", "label")

    def __init__(self, src: Entity, tgt: Entity, label: str):
        self.src = src
        self.tgt = tgt
        self.label = label


def mask_emails(md: str) -> str:
    """Blank out emails so domain scanning can't pick up a local-part or host token."""
    return RE_EMAIL.sub(lambda m: " " * len(m.group(0)), md)


def extract_entities(md: str) -> List[Entity]:
    """Typed entity extraction, deduped per (type, value). Mirrors maltego.rs."""
    seen = set()
    out: List[Entity] = []

    def push(ty: str, value: str):
        v = value.strip()
        if not v:
            return
        key = (ty, v)
        if key not in seen:
            seen.add(key)
            out.append(Entity(ty, v))

    # Primary subject: first "subject:"/"name:" line.
    for line in md.splitlines():
        low = line.lower()
        if "subject" in low or "name:" in low:
            parts = line.split(":", 1)
            if len(parts) > 1:
                push("Person", parts[1])
            break

    email_hosts = set()
    for m in RE_EMAIL.finditer(md):
        push("Email", m.group(0))
        host = m.group(0).rsplit("@", 1)[-1]
        email_hosts.add(host.lower())

    for m in RE_PHONE.finditer(md):
        push("Phone", m.group(0))
    for m in RE_IP.finditer(md):
        push("IPv4Address", m.group(0))

    masked = mask_emails(md)
    for m in RE_DOMAIN.finditer(masked):
        push("Domain", m.group(0))
    for host in sorted(email_hosts):
        push("Domain", host)

    return out


def _label_for(ty: str) -> str:
    return {
        "Email": "email",
        "Phone": "phone",
        "Domain": "domain",
        "IPv4Address": "ip",
    }.get(ty, "linked")


def build_edges(entities: List[Entity], md: str) -> List[Edge]:
    """Relationship model: subject anchor + email→domain + domain↔IP co-location."""
    edges: List[Edge] = []
    seen = set()

    def add(src: Entity, tgt: Entity, label: str):
        if src.value == tgt.value:
            return
        key = (src.value, tgt.value, label)
        if key not in seen:
            seen.add(key)
            edges.append(Edge(src, tgt, label))

    def find(ty: str, value: str) -> Optional[Entity]:
        for e in entities:
            if e.ty == ty and e.value.lower() == value.lower():
                return e
        return None

    subject = next((e for e in entities if e.ty == "Person"), None)
    if subject is not None:
        for e in entities:
            if e != subject:
                add(subject, e, _label_for(e.ty))

    for e in entities:
        if e.ty == "Email":
            host = e.value.rsplit("@", 1)[-1]
            dom = find("Domain", host)
            if dom is not None:
                add(e, dom, "email-domain")

    domains = [e for e in entities if e.ty == "Domain"]
    ips = [e for e in entities if e.ty == "IPv4Address"]
    if domains and ips:
        for line in md.splitlines():
            on_line = [d for d in domains if d.value in line]
            if not on_line:
                continue
            for ip in (i for i in ips if i.value in line):
                for d in on_line:
                    add(d, ip, "co-located")

    return edges


# ── Case DB access ───────────────────────────────────────────────────────────
def default_db_path() -> str:
    """The badbitch-rs case store (Rust uses a `_rs.sqlite` sibling). Override with
    the BADBITCH_DB env var."""
    env = os.environ.get("BADBITCH_DB")
    if env:
        return os.path.expanduser(env)
    return os.path.expanduser("~/.local/share/badbitch/osint_cases_rs.sqlite")


def load_dossier(property_id: str, db_path: Optional[str] = None) -> Optional[str]:
    """Return the dossier_md for a case, or None if not found / DB missing."""
    path = db_path or default_db_path()
    if not os.path.exists(path):
        return None
    conn = sqlite3.connect(path)
    try:
        row = conn.execute(
            "SELECT dossier_md FROM cases WHERE property_id=?", (property_id,)
        ).fetchone()
    finally:
        conn.close()
    return row[0] if row else None


def graph_for_case(property_id: str, db_path: Optional[str] = None) -> Tuple[List[Entity], List[Edge]]:
    md = load_dossier(property_id, db_path)
    if md is None:
        return [], []
    ents = extract_entities(md)
    return ents, build_edges(ents, md)

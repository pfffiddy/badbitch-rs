# badbitch-rs Maltego local transforms

Standalone [maltego-trx](https://github.com/MaltegoTech/maltego-trx) **local
transforms** that turn badbitch-rs cases (and raw text) into live Maltego
graphs. They are intentionally *not* wired into the Rust crate — this is a
separate, optional Python project you run from Maltego Desktop. The Rust
exporter (`export_to_maltego`) writes static CSV/DOT; these transforms give you
the interactive, click-to-expand path that shares the **exact same entity and
relationship model**.

## What's here

| File | Purpose |
|------|---------|
| `transforms/badbitch_common.py` | Entity + relationship extraction (a port of `badbitch/src/tool/maltego.rs`). No third-party deps. |
| `transforms/badbitch_case_expand.py` | `BadbitchCaseExpand` — input a `property_id`, expands the saved case into a graph from the SQLite store. |
| `transforms/badbitch_expand_entity.py` | `BadbitchExpandEntity` — input any indicator (email/domain/phone/IP/name), returns its 1-hop neighbors across **all** saved cases, each link labelled with the relationship + source `property_id`. |
| `transforms/badbitch_extract_entities.py` | `BadbitchExtractEntities` — extracts entities from raw text (a paste, WHOIS blob, page dump). |
| `project.py` | maltego-trx runner. |
| `requirements.txt` | `maltego-trx`. |

### Relationship model (identical to the Rust exporter)

1. **Subject anchor** — the primary `Person` links to every other entity, with a
   typed label (`email` / `phone` / `domain` / `ip`).
2. **Derived `email → domain`** — each email links to the `Domain` of its
   `@host` (a fact, not a guess).
3. **Co-location `domain ↔ IP`** — a domain and an IPv4 on the **same source
   line** are linked (`co-located`), e.g. "acme.com resolves to 203.0.113.42".

Email local-parts (`jane.public` in `jane.public@…`) are masked before domain
scanning, so they never leak in as spurious `Domain` nodes.

## Install

```bash
cd maltego_transforms
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python3 project.py list        # should list BadbitchCaseExpand + BadbitchExtractEntities
```

The transforms read the badbitch-rs case store. By default that's
`~/.local/share/badbitch/osint_cases_rs.sqlite` (the Rust tool's `_rs.sqlite`
sibling). Point elsewhere with an env var:

```bash
export BADBITCH_DB=/path/to/osint_cases_rs.sqlite
```

## Register in Maltego Desktop (local transforms)

Maltego → **Transforms** tab → **Local Transforms** → **New Local Transform**,
then for each one:

**BadbitchCaseExpand**
- Transform ID / display name: `badbitch.case.expand` / *BadBitch: Expand Case*
- Input entity type: `maltego.Phrase` (or `maltego.Unknown`)
- Command: `python3` (or the venv's python, e.g. `.../maltego_transforms/.venv/bin/python3`)
- Parameters: `project.py local BadbitchCaseExpand`
- Working directory: the absolute path to `maltego_transforms/`

**BadbitchExpandEntity**
- Transform ID / display name: `badbitch.expand.entity` / *BadBitch: Expand Entity*
- Input entity type: `maltego.Phrase` (or any entity carrying the indicator value)
- Command: `python3`
- Parameters: `project.py local BadbitchExpandEntity`
- Working directory: the absolute path to `maltego_transforms/`

**BadbitchExtractEntities**
- Transform ID / display name: `badbitch.extract.entities` / *BadBitch: Extract Entities*
- Input entity type: `maltego.Phrase`
- Command: `python3`
- Parameters: `project.py local BadbitchExtractEntities`
- Working directory: the absolute path to `maltego_transforms/`

If you set `BADBITCH_DB`, set it in the environment Maltego launches from, or
hardcode the path by exporting it in a small wrapper script used as the Command.

## Use

1. **Expand a case:** drop a **Phrase** entity on the graph, set its value to a
   saved `property_id` (e.g. `midland-tx-123`), right-click → *BadBitch: Expand
   Case*. You get the Person/Email/Phone/Domain/IPv4 nodes with link labels.
2. **Extract from text:** drop a **Phrase** entity, paste a fragment (WHOIS,
   page text), right-click → *BadBitch: Extract Entities*.

## Relationship to `export_to_maltego` / `export_to_neo4j`

The Rust tools and these transforms are front-ends to one shared model
(`badbitch/src/tool/graph.rs` ↔ `badbitch_common.py`):

- `export_to_maltego property_id="…"` → static `*.maltego.entities.csv`,
  `*.maltego.links.csv`, `*.graphviz.dot/.png` for import or sharing.
- `export_to_neo4j property_id="…"` → a `*.neo4j.cypher` script of `MERGE`
  statements for Neo4j link analysis.
- These transforms → the same graph, built interactively inside Maltego, with
  click-to-expand from a `property_id` (or any indicator, via
  `BadbitchExpandEntity`).

Because `badbitch_common.py` mirrors `maltego.rs`, both paths yield the same
entities and the same typed edges. If you change one extractor, change the
other (the Rust unit tests and the parity check in this project's history cover
the shared sample).

## Notes / scope

- These are **local** transforms (run on your machine, read your local DB) —
  appropriate for private OSINT. No data leaves the host.
- maltego-trx also supports a TDS/server deployment (`project.py runserver`); it
  works but is out of scope here — local is the dependency-light default.
- Entity extraction is regex-based and deliberately conservative. Treat the
  graph as leads to corroborate against primary records, never ground truth.

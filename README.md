# badbitch-rs

A full-spectrum **OSINT agent** in Rust — a port of the original `badbitch2.py`.
It drives a local LLM (via [Ollama](https://ollama.com)) through a tool-calling
loop to build intelligence dossiers on **authorized** targets (people,
properties, domains, infrastructure) from public records and openly accessible
sources, then saves and exports the result.

> **Authorized use only.** This is built for legitimate investigative work
> (your own assets, consented engagements, public-records research). Entity
> extraction is regex-based and conservative — treat everything it surfaces as
> *leads to corroborate against primary records*, never ground truth.

## Why Rust

The Python original works, but the Rust port is faster, has a single static
binary, real types around the tool/agent layer, and async I/O (Tokio + reqwest)
so recon fans out cleanly. The design doctrine is unchanged: **structured data
first** (prefer JSON APIs over scraping), every integration **degrades
gracefully** (a missing API key or CLI tells you exactly what to install), and a
**hard iteration cap + history compaction** keep long cases from blowing the
context window.

## Workspace layout

```
badbitch-rs/
├── badbitch/              # the agent binary
│   └── src/
│       ├── main.rs        # CLI, REPL, single-shot, case management
│       ├── agent.rs       # model + tool loop, continuations, TL;DR summary
│       ├── ollama.rs      # /api/chat client + message types
│       ├── config.rs      # INI config (Python-compatible fallbacks)
│       ├── http.rs        # retry/backoff, rate-limit, Tor, extract, fetch-cache
│       ├── compact.rs     # token-aware history compaction
│       ├── classify.rs    # target classification (person vs address vs domain…)
│       ├── store.rs       # SQLite case store (separate *_rs.sqlite)
│       ├── recovery_calls.rs  # recover tool calls models leak as text
│       └── tool/          # the tools, grouped by domain
│           ├── web.rs  corpus.rs  people.rs  entity.rs  infra.rs
│           ├── geo.rs  property.rs  links.rs  shell.rs  dossier.rs
│           ├── graph.rs     # shared entity + relationship model
│           └── maltego.rs  neo4j.rs   # graph exporters
├── badbitch-macros/       # #[tool(...)] proc-macro (schema + Tool impl)
├── maltego_transforms/    # standalone maltego-trx local transforms (Python)
├── packaging/             # .deb maintainer scripts (postinst, badbitch-setup)
└── scripts/               # build-deb.sh, install.sh
```

## Build & run

```bash
cargo build --release
./target/release/badbitch                 # interactive REPL
./target/release/badbitch "123 Main St, City ST"   # single-shot, then exit
```

### CLI

| Command | Description |
|---|---|
| `badbitch` | interactive REPL (default) |
| `badbitch "<query>"` | single-shot query |
| `badbitch --list-tools` | list tools + which API keys / CLIs are live |
| `badbitch --list-cases` | list saved cases |
| `badbitch --show-case <id>` | print a saved dossier |
| `badbitch --export <id> [--out path.md]` | export a dossier to Markdown |
| `badbitch --init-config` | write a template `config.ini` |
| `-v`, `--verbose` | per-tool timing, retries, rate-limit waits |

In the REPL, `/reset` clears history + collected docs; `exit`/`quit` leaves.

## Setup

1. **Ollama + a model.** Install Ollama and pull a tool-capable model. The
   config defaults target a local abliterated/uncensored Qwen3 GGUF; any model
   with native tool-calls works, and the agent also recovers tool calls that
   weaker models leak as text.
2. **`badbitch --init-config`** → writes `~/.config/badbitch/config.ini`. Fill in
   the `[api_keys]` you have (all optional).
3. **SearXNG** running locally for `web_search` (default
   `http://127.0.0.1:8888/search`).
4. **Optional CLIs** for the tools that shell out: `sherlock`, `holehe`,
   `theHarvester`, `phoneinfoga`, `exiftool`, `dig`/`whois`, and `python3` +
   Playwright for `fetch_rendered`. Each tool reports its own install hint when
   missing — see `--list-tools`.

### Configuration notes

- **Keys**: live in `config.ini` under `[api_keys]`. The file is gitignored.
- **`num_ctx`**: keep ≤ 20480 on a 12 GB GPU; past ~24576 the KV cache spills to
  RAM and output degrades. Compaction is token-aware, so a smaller window just
  prunes older turns sooner.
- **Separate DB**: the case store is a sibling `*_rs.sqlite` so it never clashes
  with the Python tool's cases.
- **Tor**: `[osint] tor = true` routes scraping through a SOCKS proxy. Off by
  default — many county/property sites block Tor exit nodes.

## Tools

`--list-tools` shows live status (✓ / ✗ no key / ✗ missing CLI). Grouped:

- **Recon / corpus** — `recon_sweep` (pre-flight: classify target, fan out
  searches, archive pages to disk), `web_search`, `collect`, `query_docs`,
  `read_doc`
- **Web fetch** — `fetch_rendered` (Playwright), `fetch_url`, `fetch_json`
- **People / social** — `people_search_links`, `social_search_links`,
  `sherlock`, `holehe`, `extract_contacts`
- **Entity / breach** — `theharvester`, `phoneinfoga`, `dehashed`,
  `rocketreach`, `opencorporates`, `breach_check` (HIBP)
- **Geo** — `geocode` (Nominatim), `imagery_links`, `suncalc` (chronolocation)
- **Property** — `find_county_portals`, `arcgis_query`, `attom_property`,
  `regrid_parcel`
- **Infra / domain** — `shodan`, `censys`, `dnsdumpster`, `virustotal`,
  `intelx`, `dns_recon`
- **Recovery / output** — `wayback`, `save_dossier`, `export_to_maltego`,
  `export_to_neo4j`
- **Links** — `reverse_image_links`, `crime_data_links`, `tor_status`
- **Shell** — `run_shell`, `python_eval`, `exif_metadata`

### Adding a tool

The `#[tool]` macro generates the schema + `Tool` impl from a typed async fn:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MyToolInput { pub target: String }

#[tool(name = "my_tool", description = "What it does and when to use it.")]
pub async fn my_tool(ctx: ToolContext, input: MyToolInput) -> String { /* … */ }
```

Then add `.route(my_mod::MyToolTool)` in `tool::toolset()`.

## Graph export (Maltego, Graphviz, Neo4j)

All exporters read the same saved case and build one shared graph model
(`badbitch/src/tool/graph.rs`), so they always agree — only the output differs.

**Relationship model** (richer than a plain star): a **subject anchor** linking
the primary person to every entity with a typed label; **derived `email→domain`**
edges; and **`domain↔IP` co-location** edges for entities sharing a source line.
Email local-parts are masked so they never leak in as Domain nodes.

- `export_to_maltego property_id="<id>"` (optional `graphviz=false`) →
  `<stem>.maltego.entities.csv`, `<stem>.maltego.links.csv`, and a Graphviz
  `<stem>.graphviz.dot` (+ `.png` when the `dot` CLI is installed).
- `export_to_neo4j property_id="<id>"` → `<stem>.neo4j.cypher`, idempotent
  `MERGE` statements (nodes Person/Email/Phone/Domain/IPv4; relationships
  `HAS_EMAIL`, `HAS_PHONE`, `ASSOCIATED_WITH`, `EMAIL_DOMAIN`, `CO_LOCATED`).
  Load with `cypher-shell -f <file>`.

For an interactive, click-to-expand workflow, `maltego_transforms/` ships
standalone [maltego-trx](https://github.com/MaltegoTech/maltego-trx) **local
transforms** sharing the exact same model (a Python port): expand a saved case,
expand any indicator across all cases, or extract entities from pasted text. See
`maltego_transforms/README.md`.

## Packaging (.deb)

Build a Debian/Ubuntu package that puts `badbitch` on `PATH` and ships a
`badbitch-setup` helper:

```bash
./scripts/build-deb.sh                 # → target/deb/badbitch_<ver>_<arch>.deb
sudo apt install ./target/deb/badbitch_*.deb
badbitch-setup                         # installs Ollama, pulls the model, adds
                                       # optional CLIs, writes the config
```

Or the one-shot from a source checkout (build → install → setup):

```bash
./scripts/install.sh
```

`badbitch-setup` honors `BADBITCH_MODEL=…` (override the model) and
`BADBITCH_SKIP_MODEL=1` (skip the multi-GB pull). The `.deb` declares the
apt-installable deps; Ollama, the model, SearXNG, and the pipx OSINT CLIs are
handled by the setup step because they can't be clean apt dependencies.

## Development

```bash
cargo test           # unit tests (extraction, relationship model, compaction…)
cargo clippy --tests # lint (the tree is kept warning-free)
cargo build
```

## Disclaimer

Provided for lawful, authorized OSINT and research. You are responsible for
complying with the terms of service of every source and with all applicable
law. The authors assume no liability for misuse.

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
│           └── maltego.rs   # Maltego/Graphviz export
├── badbitch-macros/       # #[tool(...)] proc-macro (schema + Tool impl)
└── maltego_transforms/    # standalone maltego-trx local transforms (Python)
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
- **Recovery / output** — `wayback`, `save_dossier`, `export_to_maltego`
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

## Maltego / Graphviz export

`export_to_maltego property_id="<id>"` turns a saved case into:

- `<stem>.maltego.entities.csv` — `Type,Value,Weight,Note` (Maltego CSV import)
- `<stem>.maltego.links.csv` — typed source→target relationships with labels
- `<stem>.graphviz.dot` — a labelled relationship graph
- `<stem>.graphviz.png` — rendered if the `dot` CLI is installed (else skipped)

**Relationship model** (richer than a plain star): a **subject anchor** linking
the primary person to every entity with a typed label; **derived `email→domain`**
edges; and **`domain↔IP` co-location** edges for entities sharing a source line.

For an interactive, click-to-expand workflow, `maltego_transforms/` ships
standalone [maltego-trx](https://github.com/MaltegoTech/maltego-trx) **local
transforms** that share the exact same model (a Python port of the Rust
exporter). See `maltego_transforms/README.md`.

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

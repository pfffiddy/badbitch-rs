# llm-desk

A local-first LLM desktop app (Rust + egui) driving **Ollama**, with an
agentic tool loop, AI-authored tools, model import/quantization — and a
**secure Android companion app that is literally the same UI**, remote-
controlling the desktop from anywhere.

```
llm-desk/
├── core/      egui-free brain: Ollama backend, agent loop, tool registry,
│              controller (authoritative state), secure remote protocol
├── ui/        the ONE egui interface (rendered from state snapshots)
├── desktop/   desktop binary = controller + ui + optional TLS server
└── android/   the SAME ui crate on Android + a TLS remote client
    ├── rust/  cdylib built with cargo-ndk
    └── app/   Gradle shell (GameActivity)
```

The desktop is always authoritative: models, conversation, tools, imports
all live there. The phone is a remote front-end — every panel, button and
feature is identical because it *is* the identical code.

---

## Desktop: build & run

Requirements: Linux, Rust **1.88+** (`rustup update stable`), a running or
installed [Ollama](https://ollama.com).

```bash
cargo run --release -p llm-desk
```

If Ollama isn't running, the app starts `ollama serve` for you.

### Parameters — the app does not tune anything

The **Parameters** panel is overrides-only. A **blank** field is *omitted
from the request entirely*, so Ollama applies its own defaults and
model-level settings. Type a value to force exactly that value. (Earlier
versions computed `num_gpu`/`num_ctx`/… from detected hardware and sent them
on every request; a wrong VRAM estimate could make Ollama fail to load a
newly selected model — which looked like "model switching is broken". That
auto-tuning is gone. The System panel remains as read-only info.)

Model switching is now an explicit, validated command: picking a model
updates the status line (`model → name`), and unknown names are rejected
with a hint to refresh.

### Tools & AI-created tools

A tool = name + description + typed parameters + a shell command template
(`{param}` placeholders, shell-escaped, run via `sh -c`). The system-prompt
block and the JSON grammar constraining the model's output regenerate
automatically — **every agent step**, which is what makes the next part work:

Ask the model to build tools for you:

> "Read ~/bin/backup.sh and create a tool that runs it with a target
> directory parameter."

The agent can research first (`read_file`, `run_shell` — e.g. probing
`--help` output), then calls the built-in **`create_tool`** meta-tool. The
definition is validated (snake_case unique name; every `{placeholder}` must
match a declared parameter; lenient JSON parsing with correctable error
messages), added to the live registry, marked 🤖 in the Tools panel, and
usable by the model **on its very next step** so it can immediately test it.

Tools persist to `~/.config/llm-desk/tools.json`.

⚠ Tools run shell commands with your privileges. `run_shell` ships disabled;
AI-created tools are enabled on creation — review the 🤖 entries, and
disable/delete anything you don't want.

### Importing models

Drop a `.gguf` file or a HuggingFace model directory onto the window (or
paste a path). HF directories are converted + quantized via your llama.cpp
checkout (path & quant level configurable in the Import panel), then
registered with Ollama. Pulling from the Ollama registry works too.

**From the phone, too.** The Model-import panel on the phone has a
**Choose a model file to upload** button (and accepts drops): it streams the
`.gguf` up the encrypted socket to the desktop, which imports it into Ollama
exactly as if you'd dropped it on the desktop window. A progress bar shows
the transfer; both screens then show the import log. (Large models over
Wi-Fi take a while — the bytes still have to cross the network.)

---

## Remote access & the Android app

### Security model (read this once)

* First enable generates a self-signed **ECDSA P-256 certificate**
  (`~/.config/llm-desk/cert.der` / `key.der`). Clients pin its **SHA-256
  fingerprint** — there is no CA, and no "accept any cert" mode after pairing.
* **Pairing**: the desktop shows a one-time 16-character code (~80 bits,
  120 s lifetime, one attempt per window). The phone never sends the code;
  it sends `HMAC-SHA256(key = code, msg = server_cert_fingerprint ||
  device_name)` — computed over the fingerprint of the certificate the phone
  *actually saw*. A man-in-the-middle presenting its own certificate changes
  that fingerprint and the MAC verifies false. Wrong guess ⇒ the window
  closes (one-strike).
* On success the phone receives a random 32-byte **device token** and the
  pinned fingerprint. The desktop stores only the token's SHA-256
  (`devices.json` is not a credential store). Tokens are compared in
  constant time and are **revocable per device** in the Remote panel.
* Every connection is TLS; unauthenticated sockets get one error frame and
  are closed. The server binds `0.0.0.0` on port **4832** (configurable
  while disabled).

### Pairing walkthrough

1. Desktop → **Remote access** panel → *Enable*. Note the listed IP
   addresses + fingerprint.
2. Click **Pair new device** → a code like `K3QN-8FWD-P2XA-M7RT` appears
   with a countdown.
3. Phone → enter host (desktop IP), port, the code, and a device name →
   **Connect**. Case and dashes don't matter.
4. Done — the phone stores host/port/fingerprint/token privately and
   reconnects with one tap from then on. The desktop lists the device and
   can revoke it any time.

### Different networks

The clean way is an overlay VPN — install **Tailscale** on both machines,
then pair/connect using the desktop's `100.x.y.z` Tailscale IP. Everything
(TLS, pinning, tokens) works unchanged, and nothing is exposed to the
internet.

Port-forwarding 4832 on your router also works — the protocol survives a
hostile network by design (pinned TLS + tokens + one-strike pairing) — but
prefer the VPN: it's less surface, and your IP isn't advertising a service.

---

## Installing the Android app

### Easiest: download the prebuilt APK

Every push builds the phone app in CI (`.github/workflows/android.yml`) and
publishes it two ways:

* the **`llm-desk-apk` prerelease** → a direct `llm-desk.apk` download (open
  it in your phone's browser and tap to install), and
* the **`llm-desk-apk` workflow artifact** on the Actions run.

On the phone you'll need to allow *Install unknown apps* for your browser or
Files app the first time. The APK is debug-signed (for easy sideloading) and
ships both `arm64-v8a` and `armeabi-v7a`, so it installs on the LG K51
(LM-K500UM, Android 10 / API 29) and any newer Android.

### Or build it yourself (target: LG K51, Android 10)

The app is built for **API 29 (Android 10)** and ships **armeabi-v7a +
arm64-v8a** — the K51 (LM-K500UM) is covered either way its firmware swings.

One-time setup:

```bash
# Rust targets
rustup target add aarch64-linux-android armv7-linux-androideabi
cargo install cargo-ndk

# Android SDK + NDK (easiest: install Android Studio, then
# SDK Manager → install "NDK (Side by side)" and a 34 platform)
export ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/<version>
```

Build:

```bash
cd android/rust
cargo ndk -t arm64-v8a -t armeabi-v7a \
  -o ../app/src/main/jniLibs build --release

cd ..
# Open `android/` in Android Studio and press Run, or:
gradle assembleDebug          # (or ./gradlew if you generate a wrapper)
adb install app/build/outputs/apk/debug/app-debug.apk
```

On the phone: enable *Install unknown apps* for your installer if
sideloading. First launch shows the connect screen — pair as above.

Version pinning that matters: the Rust `android-activity = 0.6` crate and
the Gradle `androidx.games:games-activity:2.0.2` dependency **must** stay in
lockstep (the crate vendors that exact native glue).

### Known Android caveats

* **Text input is the roughest edge.** The app pops the soft keyboard when a
  field gains focus, but egui/winit IME support on Android is beta-quality:
  plain typing works; autocorrect/swipe/compose sequences may not. Fine for
  prompts and pairing codes.
* State sync is full-snapshot (throttled). Very long transcripts make
  updates chattier over slow links; *Clear* resets that. Deltas are the
  obvious future upgrade.
* If two clients edit the same text field simultaneously, last write wins.

---

## Where things live

| Path | What |
|---|---|
| `~/.config/llm-desk/tools.json` | tool registry (incl. AI-created) |
| `~/.config/llm-desk/devices.json` | paired devices (token *hashes*) |
| `~/.config/llm-desk/cert.der`, `key.der` | server TLS identity |
| `~/.config/llm-desk/uploads/` | model files received from a phone (before import) |
| phone: app-private `server.json` | host, port, pinned fp, device token |
| phone: app cache | staged copy of a picked model, pre-upload |

## Troubleshooting

* **"unknown model … refresh the list"** — the model list was stale; hit ⟳.
* **Server won't start** — port in use, or <1024. Change it while disabled.
* **Phone: "certificate fingerprint mismatch"** — the desktop's cert changed
  (config dir wiped?) or something is intercepting. Re-pair deliberately via
  *Forget server* only if you know why.
* **Pairing code rejected** — window expired or one wrong attempt closed it;
  click *Pair new device* again.

## Note on this revision

* Fixed a syntax error in the Parameters panel that broke the `ui` build.
* Added **model upload from the phone**: a chunked-binary channel over the
  existing TLS WebSocket (`UploadBegin` / binary frames / `UploadEnd`), a
  desktop-side receiver that stages the file and runs the normal import
  pipeline, and an Android document-picker bridge. Covered by an end-to-end
  loopback test in `core/tests/remote_roundtrip.rs` (pair → drive → upload →
  import).
* Added CI: `ci.yml` builds + tests the workspace; `android.yml` builds the
  installable APK and publishes it.

The `core` crate (agent, tools, controller, protocol, TLS server/client,
pairing, upload) is covered by unit + integration tests, including the
pairing security properties. The Android document-picker/JNI path is built
in CI but should be smoke-tested on a real device.

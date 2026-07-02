#!/usr/bin/env bash
# badbitch-rs shell helpers.
#
# Install: add this line to your ~/.bashrc (or ~/.zshrc), then open a new shell:
#     source ~/badbitch-rs/scripts/shell-aliases.sh
#
# Provides:
#   bb3            launch the agent (the installed /usr/bin/badbitch)
#   bbc [--hard]   free RAM/VRAM for the local AI, keep everything badbitch needs
#                  alive, then open btop (unless it's already running)
#
# Safe mode (default): closes a curated list of heavy desktop apps (browsers, etc.).
# Hard mode (--hard):  TERMs every process YOU own except the protect-list below and
#                      this terminal's own process tree. More aggressive, more risk —
#                      it can close background user apps/applets. It will NOT touch the
#                      AI stack, the desktop/session, networking, or this terminal.

# ── bb3: run the agent ────────────────────────────────────────────────────────
# The .deb installs the binary to /usr/bin/badbitch, so this is all it needs to be.
alias bb3='badbitch'

# ── bbc: free resources without harming badbitch ──────────────────────────────
# Everything badbitch USES or NEEDS — never killed by bbc. Matched case-insensitively
# against process names. Covers: the model server (ollama), the agent itself, SearXNG
# + its Docker host (web_search), Tor (optional scraping proxy), the transient CLIs the
# tools shell out to (python3 = playwright/theHarvester, dig/whois/nmap/…), plus the
# desktop session, networking, audio and dbus so the machine stays usable.
BBC_PROTECT='ollama|badbitch|btop|dockerd|containerd|docker-proxy|docker|tor|searxng|uwsgi|granian|python3|node|dig|whois|nmap|sherlock|holehe|theharvester|phoneinfoga|exiftool|graphviz|dot|sshd|systemd|dbus|Xorg|Xwayland|wayland|gnome-shell|gnome-terminal|konsole|xterm|kitty|alacritty|tmux|kwin|plasmashell|xfwm4|sddm|gdm|lightdm|NetworkManager|wpa_supplicant|pipewire|pulseaudio|bash|zsh|sh'

_bbc_keep_services() {
  # Ollama is vital — (re)start it if it isn't answering.
  if ! curl -fsS http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
    echo "[bbc] starting ollama…"; (ollama serve >/tmp/bbc-ollama.log 2>&1 &)
  fi
  # If a SearXNG Docker container exists but is stopped, start it (web_search needs it).
  if command -v docker >/dev/null 2>&1 \
     && docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx searxng \
     && ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx searxng; then
    echo "[bbc] starting searxng container…"; docker start searxng >/dev/null 2>&1 || true
  fi
}

_bbc_open_btop() {
  if pgrep -x btop >/dev/null 2>&1; then
    echo "[bbc] btop already running — not opening a second one."
  elif command -v btop >/dev/null 2>&1; then
    btop
  else
    echo "[bbc] btop not installed → sudo apt install -y btop"
  fi
}

bbc() {
  local hard=0
  [ "$1" = "--hard" ] && hard=1

  # Protect this terminal: walk our own ancestor PIDs so the shell + terminal emulator
  # (and anything above them) are never killed.
  local protect_pids="" p=$$
  while [ "${p:-0}" -gt 1 ]; do
    protect_pids="$protect_pids $p"
    p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
  done

  local pid nm c=0

  if [ "$hard" -eq 1 ]; then
    echo "[bbc] HARD mode — closing your non-essential processes (AI stack + this terminal kept)…"
    local comm
    while read -r pid comm; do
      case " $protect_pids " in *" $pid "*) continue ;; esac   # our own tree
      [ "$pid" = "$$" ] && continue
      printf '%s' "$comm" | grep -qiE "$BBC_PROTECT" && continue
      kill -TERM "$pid" 2>/dev/null && { echo "  closed $comm ($pid)"; c=$((c + 1)); }
    done < <(ps -u "$(id -un)" -o pid=,comm= 2>/dev/null)
    sleep 2
  else
    echo "[bbc] closing heavy desktop apps to free RAM/VRAM (safe mode; --hard for more)…"
    # Substring, case-insensitive match (via pgrep -i) so variants are caught too —
    # e.g. "firefox" also matches Kali's "firefox-esr" and "firefox-bin".
    local apps="chrome chromium firefox brave opera vivaldi msedge \
                discord slack telegram signal element spotify steam \
                thunderbird soffice obs zoom teams code"
    local a killed=""
    for a in $apps; do
      printf '%s' "$a" | grep -qiE "$BBC_PROTECT" && continue   # never a protected name
      for pid in $(pgrep -i "$a" 2>/dev/null); do
        case " $protect_pids " in *" $pid "*) continue ;; esac
        nm=$(ps -o comm= -p "$pid" 2>/dev/null) || continue
        printf '%s' "$nm" | grep -qiE "$BBC_PROTECT" && continue
        kill -TERM "$pid" 2>/dev/null && { echo "  closed $nm ($pid)"; killed="$killed $pid"; c=$((c + 1)); }
      done
    done
    sleep 2
    for pid in $killed; do kill -KILL "$pid" 2>/dev/null; done   # hard-kill any stragglers
  fi

  echo "[bbc] closed $c process(es)."
  _bbc_keep_services
  _bbc_open_btop
}

#!/usr/bin/env bash
# Install Jackstay for this user (no root needed):
#
#   curl -fsSL https://raw.githubusercontent.com/odclcslkrpb5k/jackstay/main/install.sh | bash
#
# with options after `bash -s --`, such as:
#
#   curl -fsSL https://raw.githubusercontent.com/odclcslkrpb5k/jackstay/main/install.sh \
#     | bash -s -- --replace-clipman --start
#
# Copies the programs to ~/.local/bin, the systemd user units and an XDG autostart entry to
# ~/.config, the tray's icons to ~/.local/share/icons, and to ~/.xsessionrc the switch that keeps
# Xfce's settings daemon from running a clipboard manager of its own. Nothing outside the home
# directory is touched, and the history in ~/.local/state/jackstay is kept by --uninstall.
#
# Environment (all optional):
#   GBK_VERSION=<hash>   install that release instead of the latest
#   GBK_FROM=<dir>       install files from <dir> instead of downloading (tools/install)
#   GBK_ANY_OS=1         skip the Ubuntu 26.04 check
#
# Everything is inside main(), called on the last line: if the download of this script is cut
# short, bash has nothing complete to run.
set -euo pipefail
IFS=$'\n\t'

main() {
  local repo=odclcslkrpb5k/jackstay
  local version=${GBK_VERSION:-}
  local programs=(jackstayd jackstay jackstay-ui)
  local units=(jackstayd.service jackstay-ui.service)
  local icons=(jackstay.svg jackstay-symbolic.svg)
  # What a release holds, SHA256SUMS last (tools/publish).
  local files=("${programs[@]}" jackstay-session "${units[@]}" jackstay.desktop "${icons[@]}"
    LICENSE THIRD-PARTY-NOTICES.md SHA256SUMS)

  say() { printf '%s\n' "$*" >&2; }
  die() { say "jackstay install: $*"; exit 1; }
  usage() {
    cat <<'EOF'
usage: install.sh [--replace-clipman] [--hotkey KEYS | --no-hotkey] [--start]
       install.sh --uninstall
  --replace-clipman  stop xfce4-clipman and turn off its autostart, as its own Quit does
                     (running xfce4-clipman again turns it back on)
  --hotkey KEYS      bind KEYS (Xfce's notation, such as '<Primary><Alt>h') to the history popup;
                     without it, Super+H is bound, unless a key already is or Super+H is taken
  --no-hotkey        bind no key to the popup
  --start            start jackstayd and jackstay-ui now, in this graphical session
  --uninstall        stop both and remove what this installed (and a --hotkey binding); the
                     history stays
Xfce's settings daemon, xfsettingsd, runs a clipboard manager of its own, which jackstayd cannot
run beside, unless XFSETTINGSD_NO_CLIPBOARD is set; this sets it in ~/.xsessionrc, for every
login from the next.
EOF
  }

  local replace_clipman=0 hotkey="" no_hotkey=0 start=0 uninstall=0
  while (($#)); do
    case "$1" in
      --replace-clipman) replace_clipman=1 ;;
      --start) start=1 ;;
      --hotkey) hotkey=${2:?--hotkey needs keys, such as "<Primary><Alt>h"}; shift ;;
      --no-hotkey) no_hotkey=1 ;;
      --uninstall) uninstall=1 ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; exit 2 ;;
    esac
    shift
  done
  [[ -z $version || $version =~ ^[0-9a-f]{12}$ ]] \
    || die "GBK_VERSION must be a 12-character build hash, got: $version"

  : "${HOME:?HOME is not set}"
  local config=${XDG_CONFIG_HOME:-$HOME/.config}
  [[ "$config" == /* ]] || config=$HOME/.config
  local bin=$HOME/.local/bin
  local data=${XDG_DATA_HOME:-$HOME/.local/share}
  [[ "$data" == /* ]] || data=$HOME/.local/share
  local theme=$data/icons/hicolor
  local icon_dir=$theme/scalable/apps
  local unit_dir=$config/systemd/user
  local autostart_dir=$config/autostart

  # systemctl --user, if there is a user manager to talk to.
  user_systemctl() {
    local IFS=' '
    if command -v systemctl >/dev/null && systemctl --user show-environment >/dev/null 2>&1; then
      systemctl --user "$@"
    else
      printf 'no systemd user manager here; skipped: systemctl --user %s\n' "$*"
    fi
  }

  # Writes stdin to $1 through a temporary file renamed over it, so a reader never sees half of
  # it, and a running program keeps the file it started from.
  write_file() {
    local dest=$1 mode=$2 tmp
    mkdir -p "$(dirname "$dest")"
    tmp=$(mktemp "$dest.tmp-XXXXXX")
    cat >"$tmp"
    chmod "$mode" "$tmp"
    mv -f "$tmp" "$dest"
  }

  # ~/.xsessionrc, which the X session sources at login (Xsession.d's 40x11-common_xsessionrc),
  # so its exports reach xfce4-session and every program it starts or restores: xfsettingsd among
  # them. A symbolic link (dotfiles kept in a repository) is followed: the file it names is
  # edited, and the link stays.
  local xsessionrc=$HOME/.xsessionrc
  [[ -L "$xsessionrc" ]] && xsessionrc=$(readlink -f "$xsessionrc")
  local begin='# jackstay begin: no clipboard manager in xfsettingsd (tools/install)'
  local end='# jackstay end'

  # ~/.xsessionrc without Jackstay's block, on stdout. A block whose end line was lost is
  # refused, not taken to run to the end of the file.
  without_block() {
    [[ -f "$xsessionrc" ]] || return 0
    # A line is compared without a carriage return, which an editor may have added.
    awk -v b="$begin" -v e="$end" '{ l = $0; sub(/\r$/, "", l) } l == b { skip = 1 } !skip { print }
      l == e { skip = 0 } END { exit skip }' "$xsessionrc" || {
      printf '%s: "%s" has no "%s" after it; fix it by hand\n' "$xsessionrc" "$begin" "$end" >&2
      return 1
    }
  }

  # Xfce's keyboard shortcuts bound to the popup, one a line.
  popup_bindings() {
    command -v xfconf-query >/dev/null || return 0
    xfconf-query -c xfce4-keyboard-shortcuts -p /commands/custom -l -v 2>/dev/null \
      | awk -v want="$bin/jackstay popup" '{ p = $1; $1 = ""; sub(/^ +/, ""); if ($0 == want) print p }'
  }

  # GTK trusts a theme's icon-theme.cache while it is newer than the theme's own directory, which
  # a file added to scalable/apps does not change: on a machine where some other install made
  # one, the tray's icon was not found and the panel showed a blank (the owner's flapper,
  # 2026-11-12). So a cache found there is made again, or, without the tool, the directory is
  # touched, which leaves the cache older and GTK reads the directory instead. Where there is no
  # cache, GTK reads the directory anyway.
  refresh_icon_cache() {
    [[ -f $theme/icon-theme.cache ]] || return 0
    if command -v gtk-update-icon-cache >/dev/null \
      && gtk-update-icon-cache --force --ignore-theme-index --quiet "$theme" 2>/dev/null; then
      return 0
    fi
    touch "$theme"
  }

  if ((uninstall)); then
    without_block >/dev/null
    user_systemctl stop "${units[@]}" || true
    local binding rest
    while IFS= read -r binding; do
      [[ -n "$binding" ]] && xfconf-query -c xfce4-keyboard-shortcuts -p "$binding" -r
    done < <(popup_bindings)
    if [[ -f "$xsessionrc" ]]; then
      rest=$(without_block)
      if [[ -n "$rest" ]]; then
        printf '%s\n' "$rest" | write_file "$xsessionrc" "$(stat -c %a "$xsessionrc")"
      elif grep -qF "$begin" "$xsessionrc"; then
        rm -f "$xsessionrc" # it held only the block
      fi
    fi
    rm -f "$autostart_dir/jackstay.desktop" "${units[@]/#/$unit_dir/}" \
      "$bin/jackstay-session" "${programs[@]/#/$bin/}" "${icons[@]/#/$icon_dir/}"
    refresh_icon_cache
    user_systemctl daemon-reload
    printf 'removed; the history is still in %s\n' "${XDG_STATE_HOME:-$HOME/.local/state}/jackstay"
    printf 'to have xfce4-clipman back, run it: xfce4-clipman &\n'
    exit 0
  fi

  # A desktop entry's Exec takes a quoted path only with escapes; refuse a home that would need
  # them.
  [[ ! "$bin" =~ [^A-Za-z0-9_./-] ]] \
    || die "$bin: a home directory with spaces or quotes is not supported"
  # Before anything is downloaded or written: a broken block refuses the whole install.
  without_block >/dev/null

  local src
  if [[ -n ${GBK_FROM:-} ]]; then
    src=$GBK_FROM
  else
    # Release binaries are built on Ubuntu 26.04 (x86_64) against its GTK and GLib.
    [[ $(uname -m) == x86_64 ]] || die "release binaries are x86_64 only (this is $(uname -m))"
    if [[ -z ${GBK_ANY_OS:-} ]]; then
      local id="" ver=""
      if [[ -r /etc/os-release ]]; then
        # shellcheck disable=SC1091 # the system's, read where it runs
        id=$(. /etc/os-release && printf '%s' "${ID:-}")
        # shellcheck disable=SC1091
        ver=$(. /etc/os-release && printf '%s' "${VERSION_ID:-}")
      fi
      [[ $id == ubuntu && $ver == 26.04 ]] || die "release binaries are built for Ubuntu 26.04;
this is ${id:-unknown} ${ver:-}. They may not start here. To try anyway: GBK_ANY_OS=1.
Or build from source: see https://github.com/$repo#building-from-source"
    fi
    command -v curl >/dev/null || die "curl is required"
    command -v sha256sum >/dev/null || die "sha256sum is required"

    local base
    if [[ -n $version ]]; then
      base=https://github.com/$repo/releases/download/$version
    else
      base=https://github.com/$repo/releases/latest/download
    fi
    src=$(mktemp -d)
    # Expanded now: `src` is local and gone by the time the EXIT trap runs.
    # shellcheck disable=SC2064
    trap "rm -rf '$src'" EXIT
    local f
    for f in "${files[@]}"; do
      curl -fsSL --proto '=https' --tlsv1.2 -o "$src/$f" "$base/$f" \
        || die "download failed: $base/$f"
    done
  fi
  # Catches corrupted or truncated downloads, and a staged directory whose files changed since
  # their sums were taken. (The sums come from the same release, so this is not a defence against
  # a compromised GitHub account.) A directory staged from a build (tools/install) has none.
  if [[ -f $src/SHA256SUMS ]]; then
    (cd "$src" && sha256sum --quiet --strict -c SHA256SUMS) || die "checksum mismatch"
  elif [[ -z ${GBK_FROM:-} ]]; then
    die "the release has no SHA256SUMS"
  fi

  local p
  for f in "${programs[@]}" jackstay-session "${units[@]}" jackstay.desktop "${icons[@]}"; do
    [[ -f $src/$f ]] || die "missing $f in $src"
  done
  # Every program runs here (GTK's libraries load before jackstay-ui's main), and all three are
  # one build: the one asked for, if one was.
  local got build=""
  for p in "${programs[@]}"; do
    chmod 755 "$src/$p"
    got=$("$src/$p" --version 2>/dev/null) || die "$p doesn't run here (missing libraries?)"
    [[ $got =~ ^$p\ ([0-9a-f]{12}|dev)$ ]] || die "$p says it is '$got'"
    [[ -z $build || ${BASH_REMATCH[1]} == "$build" ]] || die "$p is build ${BASH_REMATCH[1]}, not $build"
    build=${BASH_REMATCH[1]}
  done
  [[ -z $version || $build == "$version" ]] || die "the programs are build $build, not $version"

  for p in "${programs[@]}"; do
    write_file "$bin/$p" 0755 <"$src/$p"
  done
  write_file "$bin/jackstay-session" 0755 <"$src/jackstay-session"
  local unit icon
  for unit in "${units[@]}"; do
    write_file "$unit_dir/$unit" 0644 <"$src/$unit"
  done
  # The tray's icon, named by its IconName and found through its IconThemePath (docs/ui.md).
  for icon in "${icons[@]}"; do
    write_file "$icon_dir/$icon" 0644 <"$src/$icon"
  done
  refresh_icon_cache
  # Starting at login turned off (the settings window, or Xfce's Session and Startup) stays off.
  local hidden=""
  if grep -qE '^Hidden *= *true *$' "$autostart_dir/jackstay.desktop" 2>/dev/null; then
    hidden='Hidden=true'
  fi
  sed "s|^Exec=jackstay-session\$|Exec=$bin/jackstay-session|" "$src/jackstay.desktop" \
    | { if [[ -n "$hidden" ]]; then sed '/^\[Desktop Entry\]$/a Hidden=true'; else cat; fi; } \
    | write_file "$autostart_dir/jackstay.desktop" 0644
  grep -qx "Exec=$bin/jackstay-session" "$autostart_dir/jackstay.desktop" \
    || die "autostart entry: Exec not set to $bin/jackstay-session"
  # Written again on every install, so it is there once, last.
  local mode=644 rest
  [[ -f "$xsessionrc" ]] && mode=$(stat -c %a "$xsessionrc")
  rest=$(without_block)
  {
    [[ -n "$rest" ]] && printf '%s\n' "$rest"
    printf '%s\nexport XFSETTINGSD_NO_CLIPBOARD=1\n%s\n' "$begin" "$end"
  } | write_file "$xsessionrc" "$mode"
  user_systemctl daemon-reload
  # A running daemon is restarted on the new binary; its history is saved on the way down.
  user_systemctl try-restart "${units[@]}"
  printf 'installed jackstayd, jackstay, jackstay-ui and jackstay-session (build %s) in %s\n' \
    "$build" "$bin"

  if [[ -n "$hotkey" ]]; then
    command -v xfconf-query >/dev/null || die "xfconf-query not found: no hotkey bound"
    xfconf-query -c xfce4-keyboard-shortcuts -p "/commands/custom/$hotkey" -n -t string \
      -s "$bin/jackstay popup"
    printf 'bound %s to the history popup\n' "$hotkey"
  elif ((!no_hotkey)) && command -v xfconf-query >/dev/null; then
    # The default, Super+H: never over a key the popup has (an upgrade changes nothing), nor
    # over one another command or a window manager action has.
    # Xfce's settings daemon may not answer (no session bus, as in a container, or outside a
    # session): then no key is bound, and the install still succeeds.
    local default='<Super>h' listing taken
    if ! listing=$(xfconf-query -c xfce4-keyboard-shortcuts -l -v 2>/dev/null); then
      printf "Xfce's keyboard shortcuts could not be read, so the history popup has no key;\n"
      printf 'bind one with --hotkey, or in jackstay settings\n'
    elif [[ -z "$(popup_bindings)" ]]; then
      taken=$(awk -v k="$default" '{ n = split($1, part, "/"); if (part[n] == k) print }' \
        <<<"$listing")
      if [[ -n "$taken" ]]; then
        printf '%s is taken, so the history popup has no key; choose one with --hotkey:\n%s\n' \
          "$default" "$taken"
      elif xfconf-query -c xfce4-keyboard-shortcuts -p "/commands/custom/$default" -n -t string \
        -s "$bin/jackstay popup"; then
        printf 'bound %s to the history popup\n' "$default"
      else
        printf 'could not bind %s to the history popup; bind one with --hotkey\n' "$default"
      fi
    fi
  fi

  if ((replace_clipman)); then
    # What clipman's own Quit does: Hidden=true in its autostart entry. Running clipman again
    # sets it back to false. Its history files are written on every copy, so stopping it loses
    # nothing.
    local entry=$autostart_dir/xfce4-clipman-plugin-autostart.desktop
    if [[ -f "$entry" ]]; then
      if grep -q '^Hidden=' "$entry"; then
        sed 's/^Hidden=.*/Hidden=true/' "$entry" | write_file "$entry" 0644
      else
        sed '/^\[Desktop Entry\]$/a Hidden=true' "$entry" | write_file "$entry" 0644
      fi
    elif [[ -f /etc/xdg/autostart/xfce4-clipman-plugin-autostart.desktop ]]; then
      sed 's/^Hidden=.*/Hidden=true/' /etc/xdg/autostart/xfce4-clipman-plugin-autostart.desktop \
        | write_file "$entry" 0644
      grep -q '^Hidden=true$' "$entry" \
        || sed -i '/^\[Desktop Entry\]$/a Hidden=true' "$entry"
    fi
    if pkill -x -u "$(id -u)" xfce4-clipman; then
      local _
      for _ in $(seq 50); do
        pgrep -x -u "$(id -u)" xfce4-clipman >/dev/null || break
        sleep 0.1
      done
      printf 'stopped xfce4-clipman and turned off its autostart\n'
    else
      printf 'xfce4-clipman was not running; its autostart is off\n'
    fi
    printf 'if clipman is also in the panel as a plugin, remove it there (Panel > Panel Preferences > Items)\n'
  fi

  # This session's xfsettingsd started before the switch was set, if it runs without it.
  local pid
  for pid in $(pgrep -x -u "$(id -u)" xfsettingsd || true); do
    if ! grep -qzx 'XFSETTINGSD_NO_CLIPBOARD=.*' "/proc/$pid/environ" 2>/dev/null; then
      printf 'xfsettingsd is running with its clipboard manager until the next login; to restart it\n'
      printf 'without one now: XFSETTINGSD_NO_CLIPBOARD=1 xfsettingsd --replace --daemon\n'
      break
    fi
  done

  if ((start)); then
    [[ -n "${DISPLAY:-}" ]] || die "DISPLAY is not set: not in a graphical session"
    "$bin/jackstay-session"
    local _
    for _ in $(seq 50); do
      if "$bin/jackstay" status >/dev/null 2>&1; then
        "$bin/jackstay" status
        exit 0
      fi
      sleep 0.2
    done
    die "jackstayd did not answer; see: journalctl --user -u jackstayd"
  fi
  printf 'it starts at the next login; to start it now: %s/jackstay-session\n' "$bin"
}

main "$@"

#!/usr/bin/env bash
set -eo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install-codex-shims.sh [--dry-run | --apply | --uninstall] [--prefix PATH]

Installs Codex PATH shims for failtrace. Dry-run is the default.

Options:
  --dry-run       Show planned actions only (default)
  --apply         Install or update the shim directory
  --uninstall     Remove only marker-owned shim files
  --prefix PATH   Override shim directory (default: ~/.failtrace/codex-shims)
  -h, --help      Show this help
EOF
  echo 'Set FAILTRACE_BIN to the absolute binary path; otherwise validate the first failtrace on PATH.'
}

mode="dry-run"
prefix="${HOME}/.failtrace/codex-shims"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dry-run)
      mode="dry-run"
      ;;
    --apply)
      if [ "$mode" = "uninstall" ]; then
        echo "error: --apply and --uninstall are mutually exclusive" >&2
        exit 64
      fi
      mode="apply"
      ;;
    --uninstall)
      if [ "$mode" = "apply" ]; then
        echo "error: --apply and --uninstall are mutually exclusive" >&2
        exit 64
      fi
      mode="uninstall"
      ;;
    --prefix)
      if [ "$#" -lt 2 ]; then
        echo "error: --prefix requires a path" >&2
        exit 64
      fi
      prefix="$2"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
  shift
done

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
dispatcher_src="${repo_root}/scripts/codex-shim-dispatcher.py"
detector_src="${repo_root}/scripts/failtrace-enforce.py"

dispatcher_name="failtrace-codex-shim-dispatcher.py"
detector_name="failtrace-detector.py"
marker_name=".failtrace-codex-shims.marker"
dispatcher_dst="${prefix}/${dispatcher_name}"
detector_dst="${prefix}/${detector_name}"
marker_dst="${prefix}/${marker_name}"

base_runners=(
  cargo rustup pytest python python3 uv poetry pipenv coverage
  vitest jest tsc npx npm pnpm yarn bun bunx go gotip failtrace
  xcodebuild
)
# `xcrun` deliberately NOT shimmed: it's invoked by build tools, xcode-select,
# and various indirect toolchain probes, so shimming it would flood soak
# telemetry without benefit. Direct `xcrun xcodebuild test` invocations still
# go through the Claude Code PreToolUse hook + the Rust detector, just not via PATH
# interception. See docs/design/xcodebuild-test-adapter.md.

is_go_version_name() {
  case "$1" in
    go1.[0-9]*|go1.[0-9]*.[0-9]*|go1.[0-9]*rc[0-9]*|go1.[0-9]*beta[0-9]*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

contains_runner() {
  local needle="$1"
  shift
  local item
  for item in "$@"; do
    if [ "$item" = "$needle" ]; then
      return 0
    fi
  done
  return 1
}

resolve_from_path_excluding_prefix() {
  local name="$1"
  local prefix_physical=""
  if [ -d "$prefix" ]; then
    prefix_physical="$(cd "$prefix" 2>/dev/null && pwd -P || true)"
  fi
  local old_ifs="$IFS"
  IFS=:
  set -- ${PATH:-}
  IFS="$old_ifs"
  local dir dir_physical candidate
  for dir in "$@"; do
    [ -n "$dir" ] || continue
    dir_physical="$(cd "$dir" 2>/dev/null && pwd -P || true)"
    if [ -n "$prefix_physical" ] && [ "$dir_physical" = "$prefix_physical" ]; then
      continue
    fi
    candidate="${dir}/${name}"
    if [ -f "$candidate" ] && [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

discover_go_versions() {
  local found=()
  local prefix_physical=""
  if [ -d "$prefix" ]; then
    prefix_physical="$(cd "$prefix" 2>/dev/null && pwd -P || true)"
  fi
  local old_ifs="$IFS"
  IFS=:
  set -- ${PATH:-}
  IFS="$old_ifs"
  local dir candidate name
  for dir in "$@"; do
    [ -n "$dir" ] || continue
    if [ -n "$prefix_physical" ] && [ "$(cd "$dir" 2>/dev/null && pwd -P || true)" = "$prefix_physical" ]; then
      continue
    fi
    for candidate in "$dir"/go1.*; do
      [ -e "$candidate" ] || continue
      [ -x "$candidate" ] || continue
      name="$(basename "$candidate")"
      if is_go_version_name "$name" && ! contains_runner "$name" "${found[@]}" "${base_runners[@]}"; then
        found+=("$name")
      fi
    done
  done
  printf '%s\n' "${found[@]}"
}

go_versions=()
while IFS= read -r item; do
  [ -n "$item" ] || continue
  go_versions+=("$item")
done < <(discover_go_versions)

runners=("${base_runners[@]}" "${go_versions[@]}")

if [ -f "${marker_dst}" ] && [ "$(sed -n '1p' "${marker_dst}")" = "failtrace-codex-shims-v1" ]; then
  while IFS= read -r line; do
    case "$line" in
      symlink=*)
        prior_runner="${line#symlink=}"
        if [ -n "$prior_runner" ] && ! contains_runner "$prior_runner" "${runners[@]}"; then
          runners+=("$prior_runner")
        fi
        ;;
    esac
  done < "${marker_dst}"
fi

toml_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

print_config_stanza() {
  local path_value
  path_value="${prefix}:${PATH:-}"
  cat <<EOF

Paste this into ~/.codex/config.toml to put the shim directory in front of PATH:

[shell_environment_policy]
inherit = "all"

[shell_environment_policy.set]
PATH = "$(toml_escape "$path_value")"
EOF
}

planned_install() {
  echo "Codex failtrace shim install plan"
  echo "  prefix: ${prefix}"
  echo "  dispatcher: ${dispatcher_dst}"
  echo "  vendored detector: ${detector_dst}"
  echo "  marker: ${marker_dst}"
  echo "  symlinks:"
  local runner
  for runner in "${runners[@]}"; do
    echo "    ${prefix}/${runner} -> ${dispatcher_name}"
  done
  if [ "$mode" = "dry-run" ]; then
    echo "Dry-run only. Re-run with --apply to install."
  fi
}

write_marker() {
  {
    echo "failtrace-codex-shims-v1"
    echo "dispatcher=${dispatcher_name}"
    echo "detector=${detector_name}"
    echo "binary=${failtrace_binary}"
    local runner
    for runner in "${runners[@]}"; do
      echo "symlink=${runner}"
    done
  } > "${marker_dst}"
}

install_shims() {
  local python_bin
  python_bin="$(resolve_from_path_excluding_prefix python3 || resolve_from_path_excluding_prefix python || true)"
  if [ -z "$python_bin" ]; then
    echo "error: could not find python3 or python on PATH outside ${prefix}" >&2
    exit 69
  fi

  local failtrace_binary
  failtrace_binary="$(PYTHONDONTWRITEBYTECODE=1 "$python_bin" - "$detector_src" "$prefix" <<'PY'
import importlib.util
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location("failtrace_identity", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
prefix = Path(sys.argv[2])
try:
    path, reason = module.resolve_failtrace(
        configured=module.configured_failtrace(prefix), excluded_dir=prefix,
    )
except Exception:
    path, reason = None, "failtrace_configuration_error"
if path is None:
    sys.stderr.write(f"error: {reason}; set FAILTRACE_BIN to this product's absolute binary path\n")
    sys.exit(78)
if "\n" in path or "\r" in path:
    sys.stderr.write("error: binary path must not contain a newline\n")
    sys.exit(78)
print(path)
PY
  )" || exit 78

  mkdir -p "${prefix}"
  {
    printf '#!%s\n' "$python_bin"
    tail -n +2 "${dispatcher_src}"
  } > "${dispatcher_dst}"
  chmod 0755 "${dispatcher_dst}"
  cp "${detector_src}" "${detector_dst}"
  chmod 0644 "${detector_dst}"

  local runner link target
  for runner in "${runners[@]}"; do
    link="${prefix}/${runner}"
    if [ -L "$link" ]; then
      target="$(readlink "$link")"
      if [ "$target" != "${dispatcher_name}" ]; then
        echo "error: refusing to replace non-owned symlink: ${link} -> ${target}" >&2
        exit 74
      fi
      continue
    fi
    if [ -e "$link" ]; then
      echo "error: refusing to replace existing non-symlink: ${link}" >&2
      exit 74
    fi
    ln -s "${dispatcher_name}" "$link"
  done

  write_marker
  echo "Installed Codex failtrace shims at ${prefix}"
  print_config_stanza
}

uninstall_shims() {
  if [ ! -f "${marker_dst}" ]; then
    echo "No marker file at ${marker_dst}; refusing to remove anything." >&2
    exit 66
  fi

  local first
  first="$(sed -n '1p' "${marker_dst}")"
  if [ "$first" != "failtrace-codex-shims-v1" ]; then
    echo "Unrecognized marker format at ${marker_dst}; refusing to remove anything." >&2
    exit 65
  fi

  local line name path target
  while IFS= read -r line; do
    case "$line" in
      symlink=*)
        name="${line#symlink=}"
        # Reject any entry that isn't a plain basename. A corrupt or
        # hand-edited marker that contains symlink=../../foo would
        # otherwise let unlink escape the shim dir and remove symlinks
        # we don't own. Empty / "." / ".." / paths with separators are
        # all rejected here.
        case "$name" in
          ""|.|..|*/*)
            echo "warning: skipping invalid marker entry: ${line}" >&2
            continue
            ;;
        esac
        path="${prefix}/${name}"
        if [ -L "$path" ]; then
          target="$(readlink "$path")"
          if [ "$target" = "${dispatcher_name}" ]; then
            rm "$path"
          fi
        fi
        ;;
    esac
  done < "${marker_dst}"

  [ -f "${dispatcher_dst}" ] && rm "${dispatcher_dst}"
  [ -f "${detector_dst}" ] && rm "${detector_dst}"
  rm "${marker_dst}"
  rmdir "${prefix}" 2>/dev/null || true
  echo "Uninstalled marker-owned Codex failtrace shims from ${prefix}"
}

case "$mode" in
  dry-run)
    planned_install
    ;;
  apply)
    planned_install
    install_shims
    ;;
  uninstall)
    uninstall_shims
    ;;
esac

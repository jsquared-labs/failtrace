#!/usr/bin/env bash
set -eo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install-claude-skill.sh [--dry-run | --apply | --uninstall] [--prefix PATH]

Installs the failtrace Claude Code skill bundle so agents in other
projects can discover it via `Skill(failtrace)`. Dry-run is the default.

Options:
  --dry-run       Show planned actions only (default)
  --apply         Install or update the skill at the prefix
  --uninstall     Remove only marker-owned files
  --prefix PATH   Override install dir (default: ~/.claude/skills/failtrace)
  -h, --help      Show this help
EOF
}

mode="dry-run"
prefix="${HOME}/.claude/skills/failtrace"

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
source_dir="${repo_root}/skills/failtrace"

marker_name=".failtrace-skill.marker"
marker_dst="${prefix}/${marker_name}"
marker_version="failtrace-skill-v1"

if [ ! -d "${source_dir}" ]; then
  echo "error: skill source not found at ${source_dir}" >&2
  exit 66
fi

contains_string() {
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

# Lexical traversal-rejection (in read_marker_managed_files) is necessary but
# not sufficient: an entry like `file=subdir/leaf` is lexically clean, but if
# `prefix/subdir` is a symlink to another directory, the eventual
# `rm "${prefix}/subdir/leaf"` will resolve through the symlink and remove a
# file outside the prefix. Walk each parent component of `rel` from prefix
# downward and refuse if any is a symlink. The leaf itself isn't checked here
# — the caller decides what to do if the destination is a symlink.
# Returns 0 if all parent components are real directories (or don't exist
# yet); 1 if any intermediate component is a symlink.
check_no_symlink_parents_under_prefix() {
  local rel="$1"
  local dir
  dir="$(dirname "$rel")"
  if [ "$dir" = "." ] || [ -z "$dir" ]; then
    return 0
  fi
  local current="${prefix}"
  local part
  local saved_ifs="$IFS"
  IFS='/'
  # shellcheck disable=SC2086
  set -- $dir
  IFS="$saved_ifs"
  for part in "$@"; do
    [ -n "$part" ] || continue
    current="${current}/${part}"
    if [ -L "$current" ]; then
      return 1
    fi
  done
  return 0
}

source_files=()
while IFS= read -r -d '' rel; do
  rel="${rel#./}"
  case "$rel" in
    *$'\n'*)
      echo "error: source filename contains a newline; refusing to proceed" >&2
      exit 65
      ;;
  esac
  source_files+=("$rel")
done < <(cd "${source_dir}" && find . -type f -not -name "${marker_name}" -print0)

if [ "${#source_files[@]}" -eq 0 ]; then
  echo "error: skill source ${source_dir} contains no files" >&2
  exit 66
fi

# Keep marker output deterministic without sending filenames back through a
# line-delimited process. Newlines are rejected above because the marker format
# is line based; every other filename byte supported by the filesystem remains
# one array element throughout this insertion sort.
sort_source_files() {
  local LC_ALL=C
  local i j value
  for ((i = 1; i < ${#source_files[@]}; i++)); do
    value="${source_files[$i]}"
    j=$((i - 1))
    while [ "$j" -ge 0 ] && [[ "${source_files[$j]}" > "$value" ]]; do
      source_files[$((j + 1))]="${source_files[$j]}"
      j=$((j - 1))
    done
    source_files[$((j + 1))]="$value"
  done
}

sort_source_files

# Populate prior_managed with file= entries from a marker-owned install.
# Returns 0 if the marker is recognized; 1 if missing or unrecognized.
# Rejects path-traversal / absolute / dotdot entries so a hand-edited
# marker can't trick uninstall into removing files outside the prefix.
prior_managed=()
read_marker_managed_files() {
  prior_managed=()
  if [ ! -f "${marker_dst}" ]; then
    return 1
  fi
  local first
  first="$(sed -n '1p' "${marker_dst}")"
  if [ "$first" != "${marker_version}" ]; then
    return 1
  fi
  local line name
  while IFS= read -r line; do
    case "$line" in
      file=*)
        name="${line#file=}"
        case "$name" in
          ""|.|..|/*|*..*|*/)
            echo "warning: skipping invalid marker entry: ${line}" >&2
            continue
            ;;
        esac
        prior_managed+=("$name")
        ;;
    esac
  done < "${marker_dst}"
  return 0
}

planned_install() {
  echo "Failtrace skill install plan"
  echo "  source: ${source_dir}"
  echo "  prefix: ${prefix}"
  echo "  marker: ${marker_dst}"
  echo "  files:"
  local rel
  for rel in "${source_files[@]}"; do
    echo "    ${prefix}/${rel}"
  done
  if [ "$mode" = "dry-run" ]; then
    echo "Dry-run only. Re-run with --apply to install."
  fi
}

write_marker() {
  if [ -L "${marker_dst}" ]; then
    echo "error: refusing to write through symlinked marker path: ${marker_dst}" >&2
    return 74
  fi
  {
    echo "${marker_version}"
    echo "source=${source_dir}"
    local rel
    for rel in "${source_files[@]}"; do
      echo "file=${rel}"
    done
  } > "${marker_dst}"
  chmod 0644 "${marker_dst}"
}

install_skill() {
  mkdir -p "${prefix}"

  local rel target

  # The marker is both the ownership authority and a write destination. Never
  # follow a leaf symlink here: it could make an external file authorize the
  # install and would redirect write_marker outside the prefix.
  if [ -L "${marker_dst}" ]; then
    echo "error: refusing to use symlinked marker path: ${marker_dst}" >&2
    exit 74
  fi

  # Marker-aware ownership check. Two cases:
  #   (1) No recognized marker — refuse to clobber any existing target.
  #   (2) Recognized marker — refuse any destination that exists but is NOT
  #       in prior_managed. A valid-but-stale marker (e.g. left over from a
  #       previous source set) does not authorize overwriting a hand-placed
  #       file the marker never claimed to own.
  if ! read_marker_managed_files; then
    if [ -e "${marker_dst}" ]; then
      echo "error: ${marker_dst} exists but is not a recognized failtrace skill marker; refusing to install" >&2
      exit 74
    fi
    for rel in "${source_files[@]}"; do
      target="${prefix}/${rel}"
      if [ -e "$target" ] || [ -L "$target" ]; then
        echo "error: refusing to replace non-owned target: ${target}" >&2
        echo "       (no failtrace skill marker at ${marker_dst}; remove the file by hand and re-run)" >&2
        exit 74
      fi
    done
  else
    # A v1 marker with no valid file= entries is internally inconsistent —
    # we can't tell what it claims to own, so treat it as corrupt and refuse.
    if [ "${#prior_managed[@]}" -eq 0 ]; then
      echo "error: ${marker_dst} has a valid header but no managed files; refusing to install" >&2
      echo "       (remove the marker file by hand to reset the install state)" >&2
      exit 74
    fi
    for rel in "${source_files[@]}"; do
      target="${prefix}/${rel}"
      if [ -e "$target" ] || [ -L "$target" ]; then
        if ! contains_string "$rel" "${prior_managed[@]}"; then
          echo "error: refusing to replace non-marker-owned target: ${target}" >&2
          echo "       (target exists but is not listed in ${marker_dst}; remove the file by hand and re-run)" >&2
          exit 74
        fi
      fi
    done
  fi

  # Before any copy, verify the destination path under prefix is safe to
  # write through. Two cases:
  #   (1) Parent component under prefix is a symlink to an arbitrary
  #       directory — cp would dereference and write outside prefix.
  #   (2) Leaf destination is a symlink to a file outside prefix — cp
  #       follows the leaf and overwrites/chmods the target. The ownership
  #       check earlier would have passed (the leaf is marker-listed) so
  #       this is the case the marker promise alone can't catch.
  # BSD cp on macOS has no --remove-destination, so the only safe move for
  # case (2) is to refuse and ask the user to clear the symlink by hand.
  local dst_check
  for rel in "${source_files[@]}"; do
    if ! check_no_symlink_parents_under_prefix "$rel"; then
      echo "error: refusing to copy through a symlinked parent under ${prefix}: ${rel}" >&2
      exit 74
    fi
    dst_check="${prefix}/${rel}"
    if [ -L "$dst_check" ]; then
      echo "error: refusing to copy over a symlink at managed destination: ${dst_check}" >&2
      echo "       (remove the symlink by hand to reset the install state)" >&2
      exit 74
    fi
  done

  # Preflight every stale marker-owned path before copying anything. If one of
  # its parents was replaced by a symlink, cleanup cannot safely proceed. Keep
  # the existing marker intact so ownership is not forgotten, and report a
  # nonzero outcome instead of claiming a complete upgrade.
  local stale
  for stale in "${prior_managed[@]}"; do
    if ! contains_string "$stale" "${source_files[@]}" \
      && ! check_no_symlink_parents_under_prefix "$stale"; then
      echo "error: refusing upgrade because stale marker-owned path has a symlinked parent: ${stale}" >&2
      echo "       (the existing ownership marker was preserved; remove the symlink by hand and re-run)" >&2
      exit 74
    fi
  done

  # Copy each source file into the prefix, creating parent dirs.
  local src dst dst_dir
  for rel in "${source_files[@]}"; do
    src="${source_dir}/${rel}"
    dst="${prefix}/${rel}"
    dst_dir="$(dirname "$dst")"
    mkdir -p "$dst_dir"
    cp "$src" "$dst"
    chmod 0644 "$dst"
  done

  # Remove previously-managed files that are no longer in the source set.
  # Symlink-parent safety was checked for the full stale set before copying.
  for stale in "${prior_managed[@]}"; do
    if ! contains_string "$stale" "${source_files[@]}"; then
      if [ -e "${prefix}/${stale}" ] || [ -L "${prefix}/${stale}" ]; then
        rm "${prefix}/${stale}"
      fi
    fi
  done

  write_marker

  # Clean up any now-empty subdirs left behind by stale-file removal.
  # `-empty -delete` won't touch dirs that still contain files we just
  # installed, so this is safe.
  find "${prefix}" -mindepth 1 -depth -type d -empty -delete 2>/dev/null || true

  echo "Installed failtrace skill at ${prefix}"
}

uninstall_skill() {
  if [ -L "${marker_dst}" ]; then
    echo "Symlinked marker path at ${marker_dst}; refusing to remove anything." >&2
    exit 74
  fi
  if [ ! -f "${marker_dst}" ]; then
    echo "No marker file at ${marker_dst}; refusing to remove anything." >&2
    exit 66
  fi
  if ! read_marker_managed_files; then
    echo "Unrecognized marker format at ${marker_dst}; refusing to remove anything." >&2
    exit 65
  fi

  # Make cleanup all-or-nothing with respect to symlink-parent safety. Leaving
  # the marker in place preserves ownership records for a later retry.
  local rel target
  for rel in "${prior_managed[@]}"; do
    if ! check_no_symlink_parents_under_prefix "$rel"; then
      echo "error: refusing uninstall because marker-owned path has a symlinked parent: ${rel}" >&2
      echo "       (the ownership marker was preserved; remove the symlink by hand and re-run)" >&2
      exit 74
    fi
  done

  for rel in "${prior_managed[@]}"; do
    target="${prefix}/${rel}"
    if [ -e "$target" ] || [ -L "$target" ]; then
      rm "$target"
    fi
  done

  rm "${marker_dst}"

  # Remove any empty subdirs, then the prefix itself if empty. `find` may
  # try to descend into the prefix after we remove its contents; ignore
  # failures since the prefix removal below is the canonical cleanup.
  find "${prefix}" -mindepth 1 -depth -type d -empty -delete 2>/dev/null || true
  rmdir "${prefix}" 2>/dev/null || true

  echo "Uninstalled marker-owned failtrace skill from ${prefix}"
}

case "$mode" in
  dry-run)
    planned_install
    ;;
  apply)
    planned_install
    install_skill
    ;;
  uninstall)
    uninstall_skill
    ;;
esac

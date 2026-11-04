#!/usr/bin/env python3
"""Test battery for the failtrace Claude Code skill installer.

Run with:

    python3 scripts/test-claude-skill-install.py

Exits 0 on full pass, 1 on any failure. Added to the pre-commit chain
alongside test-codex-shims.py so installer regressions are caught at
commit time.

The installer (scripts/install-claude-skill.sh) writes to a configurable
prefix (default ~/.claude/skills/failtrace), so every test scopes itself
to a unique temp directory via --prefix. No test touches the real user
skill registry.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install-claude-skill.sh"
SOURCE_DIR = ROOT / "skills" / "failtrace"
MARKER_NAME = ".failtrace-skill.marker"
MARKER_VERSION = "failtrace-skill-v1"


def source_files() -> list[Path]:
    return sorted(
        path.relative_to(SOURCE_DIR)
        for path in SOURCE_DIR.rglob("*")
        if path.is_file() and path.name != MARKER_NAME
    )


def run_installer(
    prefix: Path, *args: str, installer: Path = INSTALLER
) -> subprocess.CompletedProcess[str]:
    cmd = [str(installer), "--prefix", str(prefix), *args]
    return subprocess.run(cmd, capture_output=True, text=True, check=False)


def expect(label: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  ok    {label}")
    else:
        print(f"  FAIL  {label}{(' :: ' + detail) if detail else ''}")
        FAILURES.append(label)


FAILURES: list[str] = []


def test_dry_run_is_default_and_writes_nothing() -> None:
    print("test_dry_run_is_default_and_writes_nothing")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        proc = run_installer(prefix)
        expect("exit 0", proc.returncode == 0, f"got {proc.returncode}: {proc.stderr}")
        expect("plan mentions source", str(SOURCE_DIR) in proc.stdout)
        expect("plan mentions prefix", str(prefix) in proc.stdout)
        expect(
            "plan lists every bundled skill file",
            all(str(prefix / rel) in proc.stdout for rel in source_files()),
        )
        expect("plan announces dry-run", "Dry-run only" in proc.stdout)
        expect("prefix not created on dry-run", not prefix.exists())


def test_apply_installs_skill_and_marker() -> None:
    print("test_apply_installs_skill_and_marker")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        proc = run_installer(prefix, "--apply")
        expect("exit 0", proc.returncode == 0, f"stderr: {proc.stderr}")
        expect("SKILL.md installed", (prefix / "SKILL.md").is_file())
        expect("marker installed", (prefix / MARKER_NAME).is_file())

        marker = (prefix / MARKER_NAME).read_text()
        expect("marker header is versioned", marker.splitlines()[0] == MARKER_VERSION)
        expect("marker records source", f"source={SOURCE_DIR}" in marker)
        expected_files = source_files()
        marker_files = sorted(
            Path(line.removeprefix("file="))
            for line in marker.splitlines()
            if line.startswith("file=")
        )
        expect("marker lists every bundled skill file", marker_files == expected_files)

        missing = [rel for rel in expected_files if not (prefix / rel).is_file()]
        mismatched = [
            rel
            for rel in expected_files
            if (prefix / rel).is_file()
            and (prefix / rel).read_bytes() != (SOURCE_DIR / rel).read_bytes()
        ]
        expect("every bundled skill file installed", not missing, str(missing))
        expect("installed bundle is byte-identical to source", not mismatched,
               str(mismatched))


def test_reapply_is_idempotent() -> None:
    print("test_reapply_is_idempotent")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        run_installer(prefix, "--apply")
        marker_before = (prefix / MARKER_NAME).read_text()
        proc = run_installer(prefix, "--apply")
        expect("second apply exit 0", proc.returncode == 0, f"stderr: {proc.stderr}")
        marker_after = (prefix / MARKER_NAME).read_text()
        expect("marker unchanged after re-apply", marker_before == marker_after)


def test_apply_refuses_non_owned_target() -> None:
    print("test_apply_refuses_non_owned_target")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        hand_placed = prefix / "SKILL.md"
        hand_placed.write_text("hand-placed content, not ours\n")
        proc = run_installer(prefix, "--apply")
        expect("exit 74 on non-owned target", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error mentions non-owned",
               "refusing to replace non-owned target" in proc.stderr)
        expect("hand-placed content preserved",
               hand_placed.read_text() == "hand-placed content, not ours\n")
        expect("no marker created", not (prefix / MARKER_NAME).exists())


def test_apply_refuses_corrupt_marker() -> None:
    print("test_apply_refuses_corrupt_marker")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        (prefix / MARKER_NAME).write_text("not-our-marker-v99\n")
        proc = run_installer(prefix, "--apply")
        expect("exit 74 on corrupt marker", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error mentions unrecognized marker",
               "not a recognized failtrace skill marker" in proc.stderr)


def test_apply_refuses_symlinked_marker_leaf() -> None:
    print("test_apply_refuses_symlinked_marker_leaf")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        external_marker = Path(tmp) / "external-marker"
        marker_content = (
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            "file=SKILL.md\n"
        )
        external_marker.write_text(marker_content)
        os.symlink(external_marker, prefix / MARKER_NAME)

        proc = run_installer(prefix, "--apply")
        expect("exit 74 on symlinked marker", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error mentions symlinked marker",
               "symlinked marker path" in proc.stderr)
        expect("external marker target untouched",
               external_marker.read_text() == marker_content)
        expect("marker symlink preserved", (prefix / MARKER_NAME).is_symlink())
        expect("no skill file copied", not (prefix / "SKILL.md").exists())


def test_uninstall_refuses_symlinked_marker_leaf() -> None:
    print("test_uninstall_refuses_symlinked_marker_leaf")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        managed = prefix / "SKILL.md"
        managed.write_text("owned content\n")
        external_marker = Path(tmp) / "external-marker"
        marker_content = (
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            "file=SKILL.md\n"
        )
        external_marker.write_text(marker_content)
        os.symlink(external_marker, prefix / MARKER_NAME)

        proc = run_installer(prefix, "--uninstall")
        expect("exit 74 on symlinked marker", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("managed file preserved", managed.read_text() == "owned content\n")
        expect("external marker target untouched",
               external_marker.read_text() == marker_content)
        expect("marker symlink preserved", (prefix / MARKER_NAME).is_symlink())


def test_uninstall_removes_everything_and_prefix() -> None:
    print("test_uninstall_removes_everything_and_prefix")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        run_installer(prefix, "--apply")
        expect("install precondition: SKILL.md exists",
               (prefix / "SKILL.md").is_file())
        expect("install precondition: references exist",
               (prefix / "references" / "pytest.md").is_file())
        proc = run_installer(prefix, "--uninstall")
        expect("exit 0", proc.returncode == 0, f"stderr: {proc.stderr}")
        expect("SKILL.md removed", not (prefix / "SKILL.md").exists())
        expect("marker removed", not (prefix / MARKER_NAME).exists())
        expect("prefix removed when empty", not prefix.exists())


def test_uninstall_refuses_without_marker() -> None:
    print("test_uninstall_refuses_without_marker")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        proc = run_installer(prefix, "--uninstall")
        expect("exit 66 without marker", proc.returncode == 66,
               f"got {proc.returncode}: {proc.stderr}")


def test_uninstall_refuses_corrupt_marker() -> None:
    print("test_uninstall_refuses_corrupt_marker")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        (prefix / MARKER_NAME).write_text("not-our-marker-v99\n")
        proc = run_installer(prefix, "--uninstall")
        expect("exit 65 on corrupt marker", proc.returncode == 65,
               f"got {proc.returncode}: {proc.stderr}")
        expect("corrupt marker preserved",
               (prefix / MARKER_NAME).read_text() == "not-our-marker-v99\n")


def test_uninstall_ignores_traversal_attempts() -> None:
    print("test_uninstall_ignores_traversal_attempts")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        # Sentinel file the malicious marker would try to remove if traversal
        # entries were honored.
        sentinel = Path(tmp) / "sentinel.keep"
        sentinel.write_text("should survive\n")

        # Hand-craft a marker that looks valid but contains a path-traversal
        # entry pointing at the sentinel. Uninstall must reject the entry
        # and leave the sentinel intact.
        marker_path = prefix / MARKER_NAME
        marker_path.write_text(
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            f"file=../../sentinel.keep\n"
            f"file=/absolute/path/should/be/rejected\n"
            f"file=SKILL.md\n"
        )
        # Also drop a marker-listed SKILL.md so the legitimate-removal path
        # exercises alongside the rejection path.
        (prefix / "SKILL.md").write_text("dummy\n")

        proc = run_installer(prefix, "--uninstall")
        expect("exit 0 on uninstall with mixed entries", proc.returncode == 0,
               f"stderr: {proc.stderr}")
        expect("sentinel survived traversal-attempt entry",
               sentinel.exists() and sentinel.read_text() == "should survive\n")
        expect("legitimate SKILL.md removed",
               not (prefix / "SKILL.md").exists())
        expect("warning emitted for traversal entry",
               "skipping invalid marker entry" in proc.stderr)


def test_apply_refuses_marker_with_no_managed_files() -> None:
    # A v1 marker with no valid file= entries is internally inconsistent —
    # we can't tell what it claims to own, so --apply must refuse.
    print("test_apply_refuses_marker_with_no_managed_files")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        (prefix / MARKER_NAME).write_text(
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            # No file= lines.
        )
        proc = run_installer(prefix, "--apply")
        expect("exit 74", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error explains the empty-marker condition",
               "no managed files" in proc.stderr)


def test_apply_refuses_marker_present_with_non_owned_target() -> None:
    # Marker is a valid v1 header listing only 'other.md' (a hypothetical
    # previous source set). The current source includes SKILL.md, which
    # happens to exist in the prefix but is NOT listed in the marker — that
    # means it was placed by something other than this installer, so --apply
    # must refuse rather than silently clobber it.
    print("test_apply_refuses_marker_present_with_non_owned_target")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        (prefix / MARKER_NAME).write_text(
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            f"file=other.md\n"
        )
        hand_placed = prefix / "SKILL.md"
        hand_placed.write_text("hand-placed, not marker-owned\n")

        proc = run_installer(prefix, "--apply")
        expect("exit 74", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error mentions non-marker-owned target",
               "non-marker-owned target" in proc.stderr)
        expect("hand-placed content preserved",
               hand_placed.read_text() == "hand-placed, not marker-owned\n")


def test_apply_removes_stale_marker_owned_file() -> None:
    # Simulates a previously-installed state where the marker lists a file
    # that's no longer in the source set (renamed / removed upstream).
    # --apply must remove the stale file as part of re-syncing to source.
    print("test_apply_removes_stale_marker_owned_file")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        (prefix / MARKER_NAME).write_text(
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            f"file=SKILL.md\n"
            f"file=stale.md\n"
        )
        (prefix / "stale.md").write_text("old content\n")

        proc = run_installer(prefix, "--apply")
        expect("exit 0", proc.returncode == 0, f"stderr: {proc.stderr}")
        expect("SKILL.md installed",
               (prefix / "SKILL.md").is_file())
        expect("stale.md removed (was in marker, not in source)",
               not (prefix / "stale.md").exists())
        new_marker = (prefix / MARKER_NAME).read_text()
        expect("new marker lists SKILL.md", "file=SKILL.md" in new_marker)
        expect("new marker does not list stale.md",
               "file=stale.md" not in new_marker)


def test_apply_preserves_marker_when_stale_cleanup_is_blocked() -> None:
    print("test_apply_preserves_marker_when_stale_cleanup_is_blocked")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        external_dir = Path(tmp) / "external"
        external_dir.mkdir()
        sentinel = external_dir / "victim.md"
        sentinel.write_text("should not be deleted\n")
        os.symlink(external_dir, prefix / "obsolete")
        marker_content = (
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            "file=SKILL.md\n"
            "file=obsolete/victim.md\n"
        )
        marker = prefix / MARKER_NAME
        marker.write_text(marker_content)
        installed = prefix / "SKILL.md"
        installed.write_text("old installed content\n")

        proc = run_installer(prefix, "--apply")
        expect("exit 74 when stale cleanup is unsafe", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error identifies blocked upgrade",
               "refusing upgrade" in proc.stderr)
        expect("ownership marker preserved byte-for-byte",
               marker.read_text() == marker_content)
        expect("existing installed file not partially upgraded",
               installed.read_text() == "old installed content\n")
        expect("external file untouched",
               sentinel.read_text() == "should not be deleted\n")


def test_apply_refuses_to_copy_over_leaf_symlink() -> None:
    # If a marker-owned leaf was replaced by a symlink to an arbitrary file
    # outside prefix, the parent-walk + ownership checks alone permit it
    # (the leaf IS listed in the marker, the leaf is intentionally ignored
    # by check_no_symlink_parents_under_prefix). cp on macOS has no
    # --remove-destination, so it would follow the symlink and overwrite
    # the external file.
    # Defense: -L check at the leaf, refuse with exit 74 before any cp.
    print("test_apply_refuses_to_copy_over_leaf_symlink")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        # Marker-owned install state listing SKILL.md, but the on-disk
        # SKILL.md is a symlink to a sentinel file outside prefix.
        (prefix / MARKER_NAME).write_text(
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            f"file=SKILL.md\n"
        )
        victim = Path(tmp) / "victim.md"
        victim.write_text("should not be overwritten\n")
        os.symlink(victim, prefix / "SKILL.md")

        proc = run_installer(prefix, "--apply")
        expect("exit 74", proc.returncode == 74,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error mentions symlink at managed destination",
               "symlink at managed destination" in proc.stderr)
        expect("victim file outside prefix untouched",
               victim.read_text() == "should not be overwritten\n")
        expect("symlink itself still present (we refused; didn't unlink)",
               (prefix / "SKILL.md").is_symlink())


def test_uninstall_refuses_to_traverse_symlinked_parent() -> None:
    # If a directory component under prefix is replaced by a symlink to an
    # arbitrary location, the marker entries that route through that
    # component MUST NOT be followed during `rm`. The lexical traversal
    # rejection in read_marker_managed_files only catches .. and absolute
    # paths; symlink-parent escape is a second class of vector.
    print("test_uninstall_refuses_to_traverse_symlinked_parent")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        prefix.mkdir(parents=True)
        # A real directory elsewhere holds a sentinel file the installer
        # must never touch.
        evil = Path(tmp) / "evil"
        evil.mkdir()
        sentinel = evil / "victim.md"
        sentinel.write_text("should not be deleted\n")
        # Replace prefix/subdir with a symlink pointing at evil/, and have
        # the marker claim ownership of subdir/victim.md.
        os.symlink(evil, prefix / "subdir")
        (prefix / MARKER_NAME).write_text(
            f"{MARKER_VERSION}\n"
            f"source={SOURCE_DIR}\n"
            f"file=SKILL.md\n"
            f"file=subdir/victim.md\n"
        )
        managed = prefix / "SKILL.md"
        managed.write_text("owned content\n")

        proc = run_installer(prefix, "--uninstall")
        expect("uninstall exits 74", proc.returncode == 74,
               f"stderr: {proc.stderr}")
        expect("error mentions the symlink parent",
               "symlinked parent" in proc.stderr)
        expect("sentinel file outside prefix survived",
               sentinel.exists()
               and sentinel.read_text() == "should not be deleted\n")
        expect("safe marker-owned file preserved on blocked uninstall",
               managed.read_text() == "owned content\n")
        expect("ownership marker preserved",
               (prefix / MARKER_NAME).is_file()
               and "file=subdir/victim.md\n"
               in (prefix / MARKER_NAME).read_text())


def test_source_filename_with_newline_is_rejected_safely() -> None:
    print("test_source_filename_with_newline_is_rejected_safely")
    with tempfile.TemporaryDirectory() as tmp:
        fake_root = Path(tmp) / "repo"
        fake_scripts = fake_root / "scripts"
        fake_source = fake_root / "skills" / "failtrace"
        fake_scripts.mkdir(parents=True)
        fake_source.mkdir(parents=True)
        fake_installer = fake_scripts / INSTALLER.name
        shutil.copy2(INSTALLER, fake_installer)
        (fake_source / "SKILL.md").write_text("ordinary source\n")
        (fake_source / "bad\nname.md").write_text("unsafe marker name\n")
        prefix = Path(tmp) / "install" / "failtrace"

        proc = run_installer(prefix, "--apply", installer=fake_installer)
        expect("exit 65 on newline filename", proc.returncode == 65,
               f"got {proc.returncode}: {proc.stderr}")
        expect("error explains newline rejection",
               "source filename contains a newline" in proc.stderr)
        expect("install prefix not created", not prefix.exists())


def test_mutually_exclusive_args_rejected() -> None:
    print("test_mutually_exclusive_args_rejected")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        proc = run_installer(prefix, "--apply", "--uninstall")
        expect("exit 64", proc.returncode == 64, f"got {proc.returncode}")
        expect("error explains mutex",
               "mutually exclusive" in proc.stderr)


def test_unknown_arg_rejected() -> None:
    print("test_unknown_arg_rejected")
    with tempfile.TemporaryDirectory() as tmp:
        prefix = Path(tmp) / "skills" / "failtrace"
        proc = run_installer(prefix, "--bogus")
        expect("exit 64", proc.returncode == 64, f"got {proc.returncode}")
        expect("error names the unknown arg",
               "--bogus" in proc.stderr)


def main() -> int:
    tests = [
        test_dry_run_is_default_and_writes_nothing,
        test_apply_installs_skill_and_marker,
        test_reapply_is_idempotent,
        test_apply_refuses_non_owned_target,
        test_apply_refuses_corrupt_marker,
        test_apply_refuses_symlinked_marker_leaf,
        test_uninstall_refuses_symlinked_marker_leaf,
        test_uninstall_removes_everything_and_prefix,
        test_uninstall_refuses_without_marker,
        test_uninstall_refuses_corrupt_marker,
        test_uninstall_ignores_traversal_attempts,
        test_apply_refuses_marker_with_no_managed_files,
        test_apply_refuses_marker_present_with_non_owned_target,
        test_apply_removes_stale_marker_owned_file,
        test_apply_preserves_marker_when_stale_cleanup_is_blocked,
        test_apply_refuses_to_copy_over_leaf_symlink,
        test_uninstall_refuses_to_traverse_symlinked_parent,
        test_source_filename_with_newline_is_rejected_safely,
        test_mutually_exclusive_args_rejected,
        test_unknown_arg_rejected,
    ]
    for test in tests:
        test()
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)} assertion(s)", file=sys.stderr)
        for label in FAILURES:
            print(f"  - {label}", file=sys.stderr)
        return 1
    print(f"\nok - all {len(tests)} test cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

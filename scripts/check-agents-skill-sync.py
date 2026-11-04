#!/usr/bin/env python3
"""check-agents-skill-sync.py — verify agent-facing docs cite the same failtrace contract.

Failtrace's core agent-facing instructions intentionally stay self-contained,
while runner-specific detail lives in bundled references loaded on demand.
This script keeps the core contract aligned with adoption.md and verifies the
progressive-disclosure bundle remains complete and compact.

**Scope of the check (be honest about what this does and doesn't do).**
The phrase check is a drift smoke test, not a semantic proof. It checks the
core skill and each copy-paste instruction block independently so surrounding
adoption prose cannot hide a missing rule. Bundle-path and word-budget checks
are structural and fail when the core skill loses a required reference or
grows past its intentional always-loaded budget.

If a phrase is missing from a doc, either the doc has drifted from the
canonical contract (fix the doc) or the contract itself changed (update
REQUIRED_PHRASES here in the same commit, with a one-line note on why).

Run with:

    python3 scripts/check-agents-skill-sync.py

Exits 0 on full pass, 1 with a per-doc failure summary otherwise. Added
to the pre-commit chain so drift is caught at commit time.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


# Phrases that MUST appear (case-insensitive) in each agent-facing doc.
# These are the load-bearing contract clauses — drift in any of them
# means the agent might miss a key operational rule.
#
# Update this list ONLY when the contract itself changes, in the same
# commit as the doc update. New phrases here imply every AGENT_DOC needs
# a corresponding edit.
REQUIRED_PHRASES = [
    # Wrap-everything rule (not just failing runs)
    "for any",
    # Canonical wrap command
    "failtrace run",
    # Output contract — JSON not raw log
    "json",
    # Exit-code rule — JSON validity over exit code
    "exit code",
    # Negative rule — don't bypass
    "do not run bare",
    # Version-first interpretation
    "schema_version",
    # Independent outcome and trust axes
    "runner_outcome",
    "parse_status",
    "failures_complete",
    # Full artifact when stdout was budgeted
    "emission.full_output_path",
    # Strict replay gate
    "determinism_warnings",
    # Recovery evidence
    "raw_log_path",
    # Safe command-object execution
    "suggested_next_commands",
    "argv",
]


CORE_SKILL = "skills/failtrace/SKILL.md"
CORE_SKILL_MAX_WORDS = 1_800
ADOPTION_GUIDE = "docs/adoption.md"
COPY_PASTE_BLOCKS = {
    "minimal project instruction": "## Minimal project instruction",
    "Cursor rule": "### Cursor rule",
}
RUNNER_REFERENCES = [
    "references/pytest.md",
    "references/vitest.md",
    "references/jest.md",
    "references/cargo-test.md",
    "references/tsc.md",
    "references/go-test.md",
    "references/xcodebuild-test.md",
    "references/command-boundaries.md",
]


def fenced_markdown_after_heading(document: str, heading: str) -> str:
    """Return the first tilde-fenced Markdown block beneath an exact heading."""

    heading_match = re.search(rf"(?m)^{re.escape(heading)}\s*$", document)
    if heading_match is None:
        raise ValueError(f"heading missing: {heading!r}")

    following = document[heading_match.end() :]
    block_match = re.search(r"(?ms)^~~~~markdown\s*\n(.*?)^~~~~\s*$", following)
    if block_match is None:
        raise ValueError(f"Markdown block missing after heading: {heading!r}")
    return block_match.group(1)


def main() -> int:
    failures: list[tuple[str, str]] = []

    core_path = ROOT / CORE_SKILL
    core_text = core_path.read_text(encoding="utf-8") if core_path.exists() else ""
    if not core_path.exists():
        failures.append((CORE_SKILL, "file missing"))
    for phrase in REQUIRED_PHRASES:
        if phrase.lower() not in core_text.lower():
            failures.append((CORE_SKILL, f"missing required phrase: {phrase!r}"))

    adoption_path = ROOT / ADOPTION_GUIDE
    adoption_text = (
        adoption_path.read_text(encoding="utf-8") if adoption_path.exists() else ""
    )
    if not adoption_path.exists():
        failures.append((ADOPTION_GUIDE, "file missing"))
    for label, heading in COPY_PASTE_BLOCKS.items():
        target = f"{ADOPTION_GUIDE}#{label.replace(' ', '-').lower()}"
        try:
            text = fenced_markdown_after_heading(adoption_text, heading).lower()
        except ValueError as error:
            failures.append((target, str(error)))
            continue
        for phrase in REQUIRED_PHRASES:
            if phrase.lower() not in text:
                failures.append((target, f"missing required phrase: {phrase!r}"))

    core_word_count = len(core_text.split())
    if core_word_count > CORE_SKILL_MAX_WORDS:
        failures.append((
            CORE_SKILL,
            f"always-loaded body is {core_word_count} words "
            f"(budget: {CORE_SKILL_MAX_WORDS})",
        ))

    for rel in RUNNER_REFERENCES:
        path = ROOT / "skills" / "failtrace" / rel
        if not path.is_file():
            failures.append((str(path.relative_to(ROOT)), "bundled reference missing"))
        if f"]({rel})" not in core_text:
            failures.append((CORE_SKILL, f"missing direct link to {rel!r}"))

    if failures:
        print("agents-skill sync check FAILED:", file=sys.stderr)
        for doc, reason in failures:
            print(f"  {doc}: {reason}", file=sys.stderr)
        print(
            "\nFix: update the doc to include the missing phrase, OR\n"
            "(if the contract itself changed) update REQUIRED_PHRASES in\n"
            "scripts/check-agents-skill-sync.py in the same commit.",
            file=sys.stderr,
        )
        return 1

    phrase_targets = 1 + len(COPY_PASTE_BLOCKS)
    phrase_checks = len(REQUIRED_PHRASES) * phrase_targets
    bundle_checks = len(RUNNER_REFERENCES) * 2
    total = phrase_checks + bundle_checks + 1
    print(
        f"ok - {phrase_checks} contract-phrase checks, "
        f"{bundle_checks} reference checks, 1 core-size check "
        f"= {total} checks ({core_word_count}/{CORE_SKILL_MAX_WORDS} words)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

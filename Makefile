.PHONY: gauntlet install-skill uninstall-skill

CANDIDATE_FAILTRACE_BIN := $(CURDIR)/target/release/failtrace
UV_CACHE_DIR ?= $(CURDIR)/.failtrace-corpus/uv-cache

gauntlet:
	cargo build --release --locked
	UV_CACHE_DIR="$(UV_CACHE_DIR)" FAILTRACE_BIN="$(CANDIDATE_FAILTRACE_BIN)" uv run scripts/corpus/run_corpus.py --check-patches
	UV_CACHE_DIR="$(UV_CACHE_DIR)" FAILTRACE_BIN="$(CANDIDATE_FAILTRACE_BIN)" uv run scripts/corpus/run_corpus.py

# Install the failtrace Claude Code skill into ~/.claude/skills/failtrace/
# so agents in any project can discover it via Skill(failtrace). Use
# `./scripts/install-claude-skill.sh --prefix PATH` to override the target.
install-skill:
	./scripts/install-claude-skill.sh --apply

# Remove the marker-owned files written by `make install-skill`. Leaves any
# non-marker-owned files in the prefix alone.
uninstall-skill:
	./scripts/install-claude-skill.sh --uninstall

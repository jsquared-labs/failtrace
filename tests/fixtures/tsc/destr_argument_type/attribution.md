upstream: unjs/destr@b427d55641a7e6e1b22271dd0956a114cb388e2e (v2.0.5)
license:  MIT
notice:   upstream MIT license retained as LICENSE.md
captured: 2026-05-16 via scripts/corpus/run_corpus.py
patch:    scripts/corpus/patches/tsc/destr/type_error.patch
scenario: scripts/corpus/corpus.yaml :: tsc_destr_type_error

Patches `src/index.ts:18` (the `warnKeyDropped(key)` call) to pass `42`
instead of `key`, producing a TS2345 (argument-type-mismatch) diagnostic
under `tsc --noEmit -p .`. destr's tsconfig.json includes only
src/index.ts, so the typecheck is single-file.

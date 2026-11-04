upstream: clap-rs/clap@3030224d254dec52f5380592a8c8363081294aa2 (v4.6.1)
license:  MIT OR Apache-2.0
notice:   redistributed under MIT; upstream notice retained as LICENSE-MIT.md
captured: 2026-05-16 via scripts/corpus/run_corpus.py
patch:    scripts/corpus/patches/cargo/clap/assertion.patch
scenario: scripts/corpus/corpus.yaml :: cargo_clap_assertion_failure

Patches `tests/macros.rs::arg::name_explicit` to assert the wrong arg id,
producing a runtime `assertion left == right failed` panic in a cargo-test
binary. Captured under default reporter, single-threaded.

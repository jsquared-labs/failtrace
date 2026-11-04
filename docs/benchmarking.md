# Benchmarking Failtrace

Failtrace's primary contract is deterministic, bounded, verifiable failure reporting. Output-size and runtime measurements are useful secondary evidence, but they are workload-dependent: structured JSON can be much smaller than a noisy build log and larger than a short diagnostic.

This document defines the methodology required before publishing benchmark claims. It intentionally contains no private-corpus measurements or headline reduction number.

## Public inputs only

Use only scenarios in [`scripts/corpus/corpus.yaml`](../scripts/corpus/corpus.yaml) or another checked-in manifest whose inputs meet all of these requirements:

- the upstream repository and exact commit are public;
- the mutation patch is checked in and applies to that pinned commit;
- the upstream license permits the captured material to be redistributed;
- the scenario command, toolchain versions, Failtrace commit, and OS are recorded; and
- every proposed published artifact has been reviewed for secrets, personal paths, device identifiers, and private data.

Never mix private local runs into a public aggregate. Do not publish a raw run artifact merely because its upstream repository is public; dependency output, local paths, and environment-derived diagnostics still require review.

## Reproduce the corpus

From a clean checkout, record the Failtrace commit and build the exact candidate:

```sh
git rev-parse HEAD
cargo build --release --locked
FAILTRACE_BIN="$PWD/target/release/failtrace" uv run scripts/corpus/run_corpus.py --check-patches
FAILTRACE_BIN="$PWD/target/release/failtrace" uv run scripts/corpus/run_corpus.py --strict
```

The run directory under `.failtrace-corpus/out/runs/` records a manifest snapshot, toolchain versions, summary, and per-scenario artifacts. Keep that directory as the audit input, but do not commit or publish it until every contained file passes the privacy and licensing review above.

If a platform cannot run a scenario, report it as skipped. Never remove skips or anomalous results from the denominator without publishing the exclusion rule and the excluded rows.

## Output-size measurements

For every scenario, record these byte counts from the same run directory:

- `raw_log_bytes`: exact file size of `log`;
- `full_json_bytes`: exact file size of the persisted `output.json`;
- `replayed_budgeted_json_bytes`: stdout bytes from `failtrace analyze --max-failures 10 <run-dir>`, excluding the stderr breadcrumb; and
- `failure_count`, `failures_emitted`, `parse_status`, `failures_complete`, selected adapter, and `truncated`.

Use file byte lengths, not character counts. Define the reduction ratio as:

```text
raw_to_json_ratio = raw_log_bytes / json_bytes
```

A ratio greater than `1.0` means the JSON representation is smaller; a ratio below `1.0` means it is larger. Report both the full-document and replayed-budget ratios. `analyze` intentionally differs from `run` in command-mode fields, so label this measurement as replayed; a claim specifically about `failtrace run` stdout must capture that stdout during the scenario itself. Do not describe either representation as a token reduction unless tokens were measured separately.

Token counts are optional and content-specific. If reported, publish the tokenizer name and exact version, count the raw log and JSON with the same tokenizer, and keep byte results alongside token results so readers can reproduce the tool-independent measurement.

## Aggregation and reporting

Publish one machine-readable row per scenario before presenting aggregates. At minimum, report:

- total raw bytes divided by total JSON bytes;
- median and interquartile range of per-scenario ratios;
- the count and percentage of scenarios where JSON is smaller;
- results grouped by adapter and by raw-log size band; and
- separate results for `failures_complete: true` and `false`, with unsupported or unparsed commands shown separately.

Do not average ratios by themselves or present only the largest wins. State the number of scenarios, failures, skips, and anomalies. A benchmark claim must link the Failtrace commit, manifest snapshot, raw result table, aggregation script, OS, and toolchain record that produced it.

## Runtime overhead

Measure runtime separately from output size. Compare the same pinned command run directly and through `failtrace run`, using identical environment and cache state. Use at least one warm-up and ten measured repetitions per scenario, alternate direct-first and wrapped-first order, and report every sample plus median and p95 wall time. Record whether dependency installation or compilation was included.

Runtime comparisons must not reuse a direct run's log as Failtrace input; `failtrace run` must perform its real capture, persistence, parsing, and serialization path. Report absolute overhead in milliseconds as well as relative overhead. Do not combine results from different operating systems into one latency number.

## What a public claim may say

A supported claim is scoped to the measured corpus, for example: “On these pinned public scenarios at commit X, budgeted Failtrace stdout was smaller than the raw log in N of M runs.” It must not imply that every runner or every failing command saves context.

Correctness and usefulness require separate evidence. Output size does not establish that the first failure is actionable, that classification is correct, or that the failure list is complete; those are evaluated through fixtures, invariants, `parse_status`, and `failures_complete`.

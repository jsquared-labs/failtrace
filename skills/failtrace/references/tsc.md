# TypeScript compiler

Use for direct and supported wrapper forms of `tsc`; this is a typecheck-only adapter.

- Every diagnostic has `phase: typecheck`. Test counts remain zero; `counts.files_failed` counts unique primary diagnostic files.
- Clean exit lowers to `passed`; nonzero exit lowers to `typecheck_failed`.
- Only error-severity diagnostics become failures in v0.1. Warning, suggestion, and message diagnostics are dropped; `companion_diagnostics` remains empty.
- Named classification covers:
  - `TS2304`, `TS2552`, `TS2307`, `TS2688` → `missing_symbol`
  - `TS2322`, `TS2345`, `TS2741`, `TS2769` → `type_error`
  - `TS1005`, `TS1128`, `TS1136` → `syntax_error`
  - other codes → `unknown` plus advisory `unstructured_diagnostic`
- Global diagnostics without a source location use `-p` / `--project` / `--build`, then a JSON positional argument, then `tsconfig.json` as the deterministic fallback location.
- `extensions.tsc.column_unit` is `utf16_code_unit`; `extensions.tsc.phase_detail` is `typecheck`. Neither participates in `failure_id`.
- `tsc -b` / `--build` supports single-project parsing. Cross-project cascade reasoning is deferred.
- Watch mode and response files are rejected. Package-script aliases are not introspected; invoke the underlying compiler directly through Failtrace.

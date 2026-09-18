# Cross-crate tests

Unit tests live beside their parsers, and the CLI's end-to-end tests are in
`apps/openrvt-cli/tests/`. Both run on synthetic CFB files built at runtime, so
they need no `.rvt` model and run in CI.

## `baseline/corpus_metrics.tsv`

The accepted decode measurements over the reference corpus, and the one thing
here that is not synthetic.

Decode accuracy cannot be measured on a synthetic file: a rule is accepted in
this project when the declarations consume a real body with nothing left over,
across the corpus. Those models are confidential and cannot be committed, so
what is committed instead is the *result* of measuring them - counts only, keyed
by the `SMALL`/`MEDIUM`/`BIG` prefix of a file's name. No bytes and no client
file names appear here.

`apps/openrvt-cli/tests/corpus_regression.rs` re-measures those numbers and fails
if any moved the wrong way. It is opt-in, because it needs the corpus:

```bash
scripts/corpus_check.sh                   # measure and compare
scripts/corpus_check.sh --write           # accept the current numbers
OPENRVT_CORPUS=/path/to/models scripts/corpus_check.sh
```

Without `OPENRVT_CORPUS` the test skips, so `cargo test --workspace` stays fast
and CI - which has no corpus - still runs everything it can.

Run it before and after any change to the record walk. The reason: a change
there can buy one class by selling another, and the
per-class table is the only thing that shows both halves.

When a change moves a number legitimately, re-accept it with `--write` and
commit the new baseline **with** the change, so the diff records what the change
bought and what it cost. A number that moved for a reason you cannot state is
the case this gate exists to stop.

Corpus-based integration tests that assert on decoded content, rather than on
aggregate counts, still wait on redistributable fixtures and their provenance
manifests - see `fixtures/README.md`.

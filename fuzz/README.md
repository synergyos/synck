# Fuzz targets

Each target runs one check from `crates/synck-fuzz`. The same checks run on
mutated spec vectors in every `cargo test -p synck-fuzz`.

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run bundle -- -max_total_time=600
```

Without nightly, the targets build on stable. They then run without sanitizers,
which is enough to find panics, hangs and excess memory use:

```bash
RUSTFLAGS="--cfg fuzzing -Cpasses=sancov-module \
  -Cllvm-args=-sanitizer-coverage-level=4 \
  -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
  -Cllvm-args=-sanitizer-coverage-pc-table \
  -Cllvm-args=-sanitizer-coverage-trace-compares" \
  cargo build --release --target "$(rustc -vV | sed -n 's/host: //p')" --bins
```

Crashes land in `fuzz/artifacts/<target>/`. Reproduce one with
`cargo +nightly fuzz run <target> <file>`, then add it to the seeds in
`crates/synck-fuzz/tests/mutated_vectors.rs` when the fix lands.

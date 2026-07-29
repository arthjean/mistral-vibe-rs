# Compatibility harness

The harness turns a pinned upstream release into auditable fixtures and
versioned verdicts without linking or shipping upstream code.

`baseline.toml` pins the release, commit, tree, Git archive digest, lockfile
digest, Python version, platform, fixture schema, and ignored checkout
location. `capability-matrix.toml` owns every inventoried surface and records
its source and test anchors, dependencies, fixture class, implementation
status, divergence status, and required release.

Provision with:

```console
cargo run -p vibe-compat -- provision --source ../mistral-vibe --sync
```

This clones the pinned commit into `target/compat/upstream`, verifies it is
clean, verifies both digests, and creates its frozen Python environment. The
source argument is cloned only; it is never executed.

Record and validate with:

```console
cargo run -p vibe-compat -- record
cargo run -p vibe-compat -- validate --corpus compat/corpus/upstream-2.23.1
```

Every recording creates a fresh frozen dependency environment without
installing upstream project code. Every scenario then runs twice inside a
Linux bubblewrap namespace with no network, read-only checkout, dependency,
Python, and harness mounts, and only its fresh run root writable. The child
audit hook additionally rejects nested processes and unexpected reads.
Only scenario-declared JSON pointers may be canonicalized. Schema-specific
fixture decoding plus broad secret, credentialed-URL, and home-path detection
run before staged fixtures are promoted atomically.

Compare an implementation corpus with:

```console
cargo run -p vibe-compat -- compare \
  --expected compat/corpus/upstream-2.23.1 \
  --actual path/to/rust-corpus \
  --report-json compatibility.json \
  --report-markdown compatibility.md \
  --release 0
```

Reports fail closed when a required matrix row lacks evidence. Intentional
divergences count only when registered on the matrix row.

# Rebaseline policy

The 2.23.1 corpus and every published 1.0 artifact are immutable. A newer
upstream release, post-release regression, failed artifact, or newly discovered
divergence opens a new baseline and change report. It never replaces existing
fixtures or artifacts in place.

A rebaseline must:

1. pin the upstream version, commit, tree, archive digest, lockfile digest,
   Python version, platform, and fixture schema;
2. provision a clean detached oracle checkout;
3. diff the complete discovered-surface inventory and assign owners, support
   classes, dependencies, and evidence paths;
4. record new fixtures through the hermetic recorder;
5. register every changed intentional divergence with rationale and
   user-visible documentation;
6. run the complete Rust corpus and five native certification suites;
7. publish a change report that references the previous immutable baseline.

Regression fixes against an existing release produce a new versioned artifact
set and report. Checksums, signatures, attestations, schemas, notices,
changelog, and rollback instructions must reference that new source revision.

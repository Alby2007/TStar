# Security policy

Report vulnerabilities through **GitHub private vulnerability
reporting** on this repository (Security → "Report a vulnerability") —
not in public issues or PRs.

The full threat model, containment guarantees, known-accepted
advisories, and reporting expectations live in
[docs/security.md](docs/security.md) — read §11 (Reporting) for what a
useful report looks like. Pre-1.0, honest bug reports about the
boundaries documented there are as welcome as exploits.

**Release candidates are unsigned.** Until code-signing is configured,
installers and binaries ship without signatures or notarization; a
quarantined download on macOS shows the Gatekeeper warning and opens
after "Open Anyway". Verify provenance via the GitHub release and the
workflow run that produced the artifact.

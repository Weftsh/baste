# Contributing

Thanks for helping make local CI trustworthy.

## Building and testing

You need a stable Rust toolchain (1.87 or newer), git 2.38+, bash, and Node.js for the JavaScript-action tests.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
gate/test.sh
```

The end-to-end tests in `crates/baste/tests/e2e.rs` push to a local bare repository, run the real binary with the `host` backend, and check the statuses a fake GitHub API receives. Most behaviour changes should come with a test there, in `crates/baste-agent/tests/jobs.rs`, or next to the code.

VM changes have their own checks:

- `scripts/qemu-smoke.sh` boots the Firecracker guest path under QEMU. It needs no KVM, so it runs on any Linux machine.
- `scripts/firecracker-e2e.sh` runs a real workflow through Firecracker. It needs KVM and `sudo baste setup-network`.
- For the Tart backend and everything that talks to the real GitHub API, follow [docs/manual-testing.md](docs/manual-testing.md) on an Apple Silicon Mac (and a Linux machine with KVM).

The website lives in `site/`: plain HTML in `site/public/` styled with Tailwind CSS v4, built with `npm ci && npm run build` and checked with `npm run check`. `npm run dev` rebuilds the stylesheet as you edit. The Pages workflow publishes it from `main`. Keep claims on the site to what Baste does today. `site/src/og.html` is the source of `site/public/og.png`, the link preview image.

Guest code must build as a static binary:

```sh
cargo build --release --target x86_64-unknown-linux-musl -p baste
```

## Fidelity first

Baste's promise is that a local pass predicts a GitHub pass. When adding support for a workflow feature, match GitHub's behaviour, including its odd corners. Anything that can't be matched should be handed to GitHub before the run starts (see `unsupported_reason` in `crates/baste-workflow/src/plan.rs`), never half-run.

## Releasing

1. Go to **Actions → Release → Run workflow** on `main` and pick patch, minor or major.
2. The run bumps the version in `Cargo.toml` and `Cargo.lock` and commits that to `main`. It then builds the Linux (x86_64, arm64) and macOS binaries and publishes the GitHub release with `SHA256SUMS`, which creates the `vX.Y.Z` tag. It points the gate action's major tag (`v1`) at the release, so `weftsh/baste/gate@v1` follows it; leave that input empty to keep the tag where it is. `install.sh` picks up the newest release.
3. When the Release run succeeds, the **npm** workflow publishes it to npm as `@weftsh/baste` (a small launcher) and `@weftsh/baste-<os>-<arch>` (the binaries), with provenance. `npm/build.mjs` builds those packages from the release archives.

Publishing uses npm Trusted Publishing, so no npm token is stored anywhere. Each package trusts `npm.yml` on npmjs.com (Settings → Trusted Publisher: GitHub Actions, `Weftsh` / `baste` with a capital W, workflow `npm.yml`, no environment, "Allow npm publish" on). A new platform package needs the same entry before its first automated publish. The workflow checks every package's trust before publishing anything and prints npm's reason if one is missing. After fixing it, run the **npm** workflow with the release's tag. Versions already on npm are skipped, so re-running is safe.

The version bump is pushed straight to `main` by the workflow. If `main` is protected, let GitHub Actions bypass that rule, or bump the version in a pull request and push the `vX.Y.Z` tag instead, which releases that commit without a bump.

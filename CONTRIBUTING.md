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
- For the Tart backend, test by hand on an Apple Silicon Mac: `baste image prepare --backend tart`, then `baste run` in a repository.

The website lives in `site/`: plain HTML in `site/public/` styled with Tailwind CSS v4, built with `npm ci && npm run build` and checked with `npm run check`. `npm run dev` rebuilds the stylesheet as you edit. The Pages workflow publishes it from `main`. Keep claims on the site to what Baste does today. `site/src/og.html` is the source of `site/public/og.png`, the link preview image.

Guest code must build as a static binary:

```sh
cargo build --release --target x86_64-unknown-linux-musl -p baste
```

## Fidelity first

Baste's promise is that a local pass predicts a GitHub pass. When adding support for a workflow feature, match GitHub's behaviour, including its odd corners. Anything that can't be matched should be handed to GitHub before the run starts (see `unsupported_reason` in `crates/baste-workflow/src/plan.rs`), never half-run.

## Releasing

1. Set the new version in `Cargo.toml` (`[workspace.package] version`) and merge it to `main`.
2. Run the **Release** workflow from the Actions tab on `main` with the tag (`v0.2.0`), or push that tag. It builds the Linux (x86_64, arm64) and macOS binaries, publishes them with `SHA256SUMS`, and creates the tag. `install.sh` picks up the newest release.
3. When run from the Actions tab, it also points the gate action's major tag (`v1`) at the release, so `weftsh/baste/gate@v1` follows it. Leave that input empty to keep the tag where it is.

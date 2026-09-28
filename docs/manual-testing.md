# Testing Baste on a real machine

CI covers the Linux Firecracker backend, the workflow engine and a fake GitHub API. This checklist covers what CI can't: the macOS (Tart) backend, statuses on real GitHub commits and pull requests, merge gating, and OS keychain secrets. Run it on an Apple Silicon Mac, and on a Linux machine with KVM if you have one.

For each step, the **Expect** line is what should happen. When something else happens, collect the evidence listed in [When something fails](#when-something-fails) before changing code.

## 0. Prerequisites

- The [GitHub CLI](https://cli.github.com), logged in: `gh auth status`.
- **macOS 14+ on Apple Silicon:** `brew install cirruslabs/cli/tart` and `softwareupdate --install-rosetta --agree-to-license`. If the Homebrew tap fails, download `tart.tar.gz` from [Tart's releases](https://github.com/cirruslabs/tart/releases), move `tart.app` to `~/Applications`, and link `~/Applications/tart.app/Contents/MacOS/tart` into a directory on your `PATH`.
- **Linux:** `/dev/kvm` access (`sudo usermod -aG kvm $USER`, then log in again) and `e2fsprogs`.
- About 20 GB of free disk for the VM image.

## 1. Install

```sh
npm install -g @weftsh/baste
baste --version
```

**Expect:** the latest version on npm (`npm view @weftsh/baste version`).

Also try the install script in a separate directory: `curl -fsSL https://raw.githubusercontent.com/weftsh/baste/main/install.sh | BASTE_INSTALL_DIR=/tmp/baste-bin sh`. **Expect:** it downloads, verifies the checksum and installs the same version. On macOS, `/tmp/baste-bin/baste-linux-aarch64` exists too.

On Linux, once: `sudo "$(command -v baste)" setup-network`. **Expect:** tap devices `baste-tap0`, `baste-tap1` and a `baste-network` systemd unit.

## 2. A sandbox repository

Use a private repository you can push to and change settings on, not a real project:

```sh
gh repo create Weftsh/baste-sandbox --private --clone && cd baste-sandbox
mkdir -p .github/workflows
cp /path/to/baste/tests/sandbox/ci.yml /path/to/baste/tests/sandbox/pr.yml .github/workflows/
echo "# sandbox" > README.md
git add -A && git commit -m "Sandbox workflows" && git push -u origin HEAD
gh secret set BASTE_DEMO --body "hello-from-github"
```

`ci.yml` has a build job (checkout, a JavaScript action, `setup-node`, Docker, an artifact), a matrix that needs it, a job that fails when a `FAIL` file is committed, a job that needs the `BASTE_DEMO` secret, and a Windows job. `pr.yml` runs on pull requests.

## 3. Doctor and init

```sh
baste doctor
baste init
```

**Expect:** every check passes (virtualization, gh login, permission to write statuses, backend). `init` installs `.git/hooks/pre-push`, lists the `baste/CI/...` contexts to require, lists `CI / windows` as staying on GitHub, and notes that jobs get your gh token.

Negative check, if you can: with `gh auth logout`, `baste init` stops, says to log in, and changes nothing. Log in again afterwards.

## 4. The VM image

```sh
time baste image prepare
```

**Expect:** it downloads the pinned image, verifies it, provisions it once (packages, Node.js, Docker, the `runner` user) and finishes without errors. Note how long it takes and how much disk it uses (`du -sh ~/.tart` on macOS, where Tart keeps the images; `du -sh ~/.cache/baste` on Linux). A second run is quick.

## 5. A passing push

```sh
baste secrets set BASTE_DEMO    # enter any value
git commit --allow-empty -m "First local run" && git push
baste status
baste logs latest
```

**Expect:**
- `git push` returns right away and prints `baste: running CI for <sha> (<branch>) locally in run <id>`.
- Within seconds the commit shows pending `baste/CI/...` checks on GitHub: `gh api repos/Weftsh/baste-sandbox/commits/$(git rev-parse HEAD)/statuses --jq '.[] | [.context, .state, .description] | @tsv'`.
- `baste logs latest` streams steps live. The build job prints `user=runner`, the JavaScript action prints the repository and commit, `setup-node` installs Node 22, and Docker reports `x86_64` (through Rosetta on a Mac).
- Every local job ends `success` with `Passed in … · baste logs <id>`. `CI / windows` gets no Baste status and runs on GitHub.
- The **Details** link on a check opens `https://weftsh.github.io/baste/run/?id=<id>&…` with the right commands.
- A desktop notification when the run finishes.

## 6. A failing push

```sh
touch FAIL && git add FAIL && git commit -m "Fail on purpose" && git push
```

**Expect:** `baste/CI/maybe-fail` turns `failure` with a description of at most 140 characters that ends in `· baste logs <id>`, and the other checks pass. `baste logs <id> --failed` shows the failing step's output and exit code 1. Then `git rm FAIL && git commit -m "Fix" && git push` turns it green.

## 7. Secrets

```sh
baste secrets rm BASTE_DEMO
git commit --allow-empty -m "No secret" && git push
```

**Expect:** `uses-a-secret` fails before running any step, with a message naming `BASTE_DEMO`. `baste secrets set BASTE_DEMO` then `baste rerun <id>` passes, and the value shows as `***` in `baste logs`. `baste secrets list` names the keychain in use (macOS Keychain, or Secret Service on Linux).

## 8. A pull request

```sh
git switch -c try-a-pr && git commit --allow-empty -m "PR change" && git push -u origin HEAD
gh pr create --fill
git commit --allow-empty -m "Another PR change" && git push
```

**Expect:** the second push also runs `PR / merge` locally, on the test merge of the branch into `main`. Its log shows `event=pull_request` and the merge commit on top. Its status appears on the branch's head commit.

## 9. Merging on a local pass

1. In the sandbox's settings, add a branch ruleset on `main` that requires the `baste/CI/build` and `baste/CI/maybe-fail` status checks.
2. On the PR from step 8, push a commit with `FAIL`. **Expect:** GitHub blocks the merge.
3. Remove `FAIL` and push. **Expect:** the PR becomes mergeable once the local checks pass.

Then the gate action: add the `baste-gate` job from the README to `ci.yml`, with `needs: baste-gate` and the `if:` on the `build` job, and push. **Expect:** after a local pass, the GitHub-hosted `build` job is skipped and the workflow passes quickly. `BASTE_DISABLE=1 git push` skips the local run, so everything runs on GitHub as usual.

## 10. Everyday commands

- `baste status --commit $(git rev-parse HEAD)` finds the run for a commit.
- Push twice quickly on the same branch. **Expect:** the older run is cancelled and its statuses say so.
- `baste cancel <id>` stops a run in progress.
- `baste insights` shows time per step and, once GitHub has run the workflow, local against GitHub durations.
- `baste uninstall` removes the hook, and the next push posts nothing.

## 11. npx

In a scratch clone, `npx @weftsh/baste init`. **Expect:** a note that the hook points into npm's cache, with `npm install -g @weftsh/baste` as the fix.

## When something fails

Collect:
- the command and its full output
- `baste status <id>` and `baste logs <id>`
- `.git/baste/runs/<id>/worker.log`, and `prepare.log` for image problems
- `baste doctor` and `baste --version`

Where the code is:
- macOS VMs: `crates/baste/src/backend/tart.rs`
- Linux VMs: `crates/baste/src/backend/firecracker.rs`
- The image setup script: `crates/baste/src/backend/provision.sh`
- Statuses: `crates/baste/src/poster.rs`, called from `worker.rs`
- The agent that runs inside the VM: `crates/baste-agent`

Fix in this repository with a test where one fits, run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`, and release through the Release workflow. To try a fix before releasing, build it (`cargo build --release -p baste`) and put `target/release/baste` first on your `PATH`. On a Mac, the VMs also need the Linux agent: put `baste-linux-aarch64` from the latest release's macOS archive next to it, or point `BASTE_AGENT_BIN` at it.

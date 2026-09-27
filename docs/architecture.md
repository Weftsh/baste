# Architecture

Baste is a single binary with no service behind it. Your machine runs the jobs, and GitHub only ever sees commit statuses.

## The flow of a push

```
git push ──▶ pre-push hook ──▶ baste hook pre-push ──▶ run record (queued)
                                      │                     │
                                      └─ spawn, detached ──▶ baste worker <run>
                                                            │
      plan ◀────────────────────────────────────────────────┘
       │  workflows at the pushed SHA, push / pull_request filters,
       │  test merge for PRs, matrix expansion, runs-on classification
       ▼
  pending statuses ──▶ status poster (retries until the commit reaches GitHub)
       │
       ▼
  scheduler: needs order, job-level if, dynamic matrices, fail-fast, slots
       │
       ▼  per job
  bundle (job spec, actions, checkout packs, artifacts)
       │
       ▼
  backend.run ──▶ fresh VM ──▶ agent ──▶ events ──▶ step logs, run record
       │
       ▼
  final status per job, insights (local vs GitHub), provenance
```

The hook returns in milliseconds and never fails the push. The worker is a separate session (`setsid`), so closing the terminal doesn't stop it. The hook runs before the push reaches GitHub, so status posts may get "no commit found" for a few seconds. The poster keeps the latest update per context and retries with backoff until the commit appears, or gives up after `status_wait_minutes` (for example, when the push was rejected).

## Crates

| Crate | Role |
| --- | --- |
| `baste-expr` | Lexer, parser and evaluator for `${{ }}` expressions, with GitHub's semantics (loose equality, case-insensitive property access, `&&`/`||` returning operands, star filters, the implicit `success()` in `if:`). |
| `baste-workflow` | YAML (1.2 core schema, anchors) to JSON, the workflow and action models, trigger and path filters, matrix expansion, `runs-on` classification, unsupported-feature detection. |
| `baste-protocol` | The runner protocol types. |
| `baste-agent` | The executor. It runs steps, actions and post steps, handles file commands and workflow commands, masks secrets, and implements the checkout and artifact shims. |
| `baste` | The CLI: hook, worker, planner, GitHub client, status poster, run store, commands, and the VM backends. The same binary is also the agent inside the VM (`baste agent ...`) and, for Firecracker, the VM's PID 1. |

## The runner protocol

A runner gets a **bundle**, a directory holding `job.json` (a `JobSpec`) and the files it references. It answers with a stream of **events**, one JSON object per line:

- `hello`
- `step_started` and `step_finished` (with outcome, conclusion and exit code)
- `log`
- `annotation`
- `summary`
- `artifact_chunk` and `artifact_end`
- `job_finished` (with the result and job outputs)

The spec is self-contained:

- raw steps and env, with expressions evaluated by the runner
- the `github`, `matrix`, `strategy`, `needs` and `vars` contexts
- secret values, and the names of any missing secrets
- downloaded actions
- git packs for checkout
- artifacts from earlier jobs
- runner facts (work directory, user, Node.js paths)

The protocol doesn't depend on the transport:

- the host backend reads the agent's stdout
- Tart reads `tart exec`'s stdout
- Firecracker reads a vsock stream

The protocol doesn't depend on the transport, so a runner elsewhere could speak it over the network.

## Run store

Runs live in `<git-common-dir>/baste/runs/<id>/`:

```
run.json                  the record: state, jobs, steps, provenance, insights (atomically replaced)
worker.log                the worker's own stderr
prepare.log               image download/provisioning output
jobs/<job-key>/steps/NNN.log   each step's output (masked by the agent)
jobs/<job-key>/summary.md      GITHUB_STEP_SUMMARY content
artifacts/<name>.tar      artifacts uploaded during the run
```

The worker is the only writer. `status` and `logs` read snapshots, and `logs` follows a run by tailing step files while polling the record. The 50 most recent runs are kept.

Machine-wide state lives in `~/.local/state/baste` (VM slot locks, Firecracker sockets), and downloads in the cache directory. Downloads are pinned images, prepared layers, actions keyed by commit, and the Firecracker binary.

## Checkout without GitHub

The pushed commit may not be on GitHub yet when the job starts, so the VM can't fetch it. Instead, the host builds a git pack for each fetch depth the job's checkout steps ask for:

- depth 1: the commit and its tree
- depth N: a breadth-first walk over parents, with the boundary written to `.git/shallow`
- depth 0: full history

The `actions/checkout` shim then does the following:

1. `git init`
2. index the pack
3. write the shallow file
4. create `refs/remotes/origin/<branch>` (or `refs/pull/N/merge`)
5. check out the commit
6. set up `origin` and credentials as the real action would

Uncommitted changes in your working tree never enter the VM.

## VM backends

Every backend boots a **fresh VM per job** from a **pinned image** with **copy-on-write**, and deletes it afterwards.

### Firecracker (Linux, WSL2)

Pinned artifacts, checked by sha256:

- Firecracker v1.12.1
- the Firecracker project's CI kernel (6.1)
- its Ubuntu 24.04 squashfs

At run time nothing needs root: the tap devices are created once by `sudo baste setup-network` and owned by your user. Each VM gets:

| Device | Contents |
| --- | --- |
| `/dev/vda` | the pinned squashfs (read-only) |
| `/dev/vdb` | the provisioned layer (read-only, built once and cached) |
| `/dev/vdc` | a sparse ext4 for this job's writes |
| `/dev/vdd` | the bundle as a raw tar (no filesystem image needed) |

The initramfs is a cpio archive Baste writes itself. Its `/init` is the static Baste binary, which does the following as PID 1:

1. mounts the three filesystems
2. stacks them with overlayfs (`lowerdir=prepared:base`)
3. bind-mounts ext4 directories for Docker's data
4. injects `baste-agent.service`, DNS and a quiet unit set
5. moves the new root over `/` and execs systemd

The service unpacks the bundle, runs the job, streams events to the host over vsock, and reboots the VM, which ends Firecracker.

The provisioned layer is made the same way. A `prepare` boot runs `provision.sh` into an empty upper disk and reboots cleanly. The host replays the ext4 journal (`e2fsck -p`) and keeps the disk as the read-only middle layer.

`scripts/qemu-smoke.sh` boots this exact path under QEMU's microvm machine without KVM. `scripts/firecracker-e2e.sh` runs a real workflow through Firecracker, and CI runs both.

### Tart (macOS on Apple Silicon)

The base is `ghcr.io/cirruslabs/ubuntu` pinned by digest. It's provisioned once into a local VM (with Rosetta for Linux registered through `binfmt_misc`). Each job:

1. `tart clone`s that VM (an APFS copy-on-write clone)
2. starts it with Rosetta and the bundle shared read-only over virtiofs
3. runs the agent with `tart exec` and reads its events
4. deletes the clone

Docker defaults to `linux/amd64` so images match GitHub's x86_64 runners.

### Host (development)

Runs the agent as a local process with no VM. The end-to-end tests use it, together with a fake GitHub API.

## Design choices

- **One runner protocol.** Every backend drives the same agent through the same protocol. There are no backend-specific code paths in the agent.
- **Routing as policy.** `routing.rs` holds the routing rule, "the pusher's machine runs the push", separate from the agent and the scheduler.
- **Provenance in the data.** Each run records where it executed: executor, host, backend, image digest, Baste version and policy. A rule like "`main` requires cloud" is then a policy change, not a migration.
- **Our own statuses, not self-hosted runners.** Workflows stay unmodified, and routing stays ours.

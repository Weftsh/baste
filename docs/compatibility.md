# Workflow compatibility

Baste runs a narrow, well-tested subset of GitHub Actions locally and hands everything else to GitHub before the run starts, so a local pass predicts a GitHub pass. This page lists what's in the subset.

## Events

| Event | Local behaviour |
| --- | --- |
| `push` | The pushed ref, with `branches`, `branches-ignore`, `tags`, `tags-ignore`, `paths` and `paths-ignore`. Changed files come from the push's `before..after` (or the merge base with the default branch for new branches). Path filters don't apply to tag pushes, as on GitHub. |
| `pull_request` | When the pushed branch has an open PR, with `branches`, `paths` and `types` (the push counts as `synchronize`). The job checks out GitHub's test merge of the head into the base, built locally from your copy of the base branch (`git fetch` keeps it fresh). If the merge conflicts, the PR's workflows stay on GitHub. |
| Anything else | Not started by a push; stays on GitHub. |

If a workflow triggers on both `push` and `pull_request` for the same push, it runs once, for `push`, and posts one status per job.

## Jobs

| Feature | Status |
| --- | --- |
| `runs-on: ubuntu-*` (any version, larger runners) | Local, in Ubuntu 24.04 |
| `runs-on: windows-*`, `macos-*`, `self-hosted`, runner groups | Handed to GitHub |
| `needs:`, `needs.<id>.outputs`, `needs.<id>.result` | Supported |
| `if:` with `success()`, `failure()`, `always()`, `cancelled()` | Supported |
| `strategy.matrix` with `include`/`exclude`, `fail-fast`, `max-parallel` | Supported, including matrices built from `fromJSON(needs...)` |
| `outputs:`, `env:`, `defaults.run`, `timeout-minutes`, `continue-on-error` | Supported |
| `services:` | Handed to GitHub |
| `container:` | Handed to GitHub |
| `environment:` | Handed to GitHub (deployments stay on GitHub) |
| `permissions: id-token: write` (OIDC) | Handed to GitHub |
| `uses:` (reusable workflows) | Handed to GitHub |
| `concurrency:` | Ignored locally. A newer push to the same branch cancels the older local run instead. |

A job that `needs` a job handed to GitHub is handed to GitHub too, since its inputs come from there.

Statuses for jobs that don't run: a job whose `if:` is false is posted as `success` ("Skipped"). A job skipped because a job it needs failed is posted as `failure` ("Not run: needs ... failed"). GitHub marks the latter as skipped, which would read as a pass.

## Steps

| Feature | Status |
| --- | --- |
| `run:` with the default shell (`bash -e`), `bash`, `sh`, `python`, or `command {0}` | Supported |
| `working-directory`, `env`, `if`, `continue-on-error`, `timeout-minutes`, `id`, `name` | Supported |
| JavaScript actions (`node12` through `node24` run on Node 24, as GitHub now does; `ACTIONS_ALLOW_USE_UNSECURE_NODE_VERSION=true` keeps Node 20) | Supported, including `pre`/`post` with `pre-if`/`post-if` |
| Composite actions (nested, with inputs and outputs) | Supported |
| Docker actions (`Dockerfile` or `docker://`) | Supported (Docker runs inside the VM) |
| Local actions (`uses: ./path`) | Supported |
| `GITHUB_ENV`, `GITHUB_OUTPUT`, `GITHUB_PATH`, `GITHUB_STATE`, `GITHUB_STEP_SUMMARY` | Supported |
| Workflow commands: `add-mask`, `group`, `error`/`warning`/`notice`, `debug`, `stop-commands`, `set-output`, `save-state` | Supported (`set-env` and `add-path` are rejected, as on GitHub) |
| Expressions: all operators, `contains`, `startsWith`, `endsWith`, `format`, `join`, `toJSON`, `fromJSON`, `hashFiles`, status functions | Supported, with GitHub's loose equality and case-insensitive property access |
| Contexts: `github`, `env`, `vars`, `secrets`, `job`, `steps`, `runner`, `strategy`, `matrix`, `needs`, `inputs` | Supported. `vars` is read from the repository when your token can. |

## Built-in actions

These actions talk to GitHub services a local run can't reach, so Baste handles them itself and falls back to the real action when inputs ask for something it doesn't cover.

| Action | Local behaviour |
| --- | --- |
| `actions/checkout` | Checks out the pushed commit (or the PR test merge) from packs Baste builds from your local repository, honouring `path`, `fetch-depth`, `clean`, `persist-credentials`, `submodules`, `lfs`. It works before the push has reached GitHub. Other repositories, other refs, `sparse-checkout`, `filter` or `ssh-key` use the real action. |
| `actions/upload-artifact` | Stores the artifact with the run (`path` globs and `!` excludes, `if-no-files-found`, `include-hidden-files`). |
| `actions/download-artifact` | Restores artifacts uploaded earlier in the same run (`name`, `pattern`, `merge-multiple`, `path`). |
| `actions/cache`, `actions/cache/restore`, `actions/cache/save` | Baste's own cache on your machine, kept per repository. Keys, `restore-keys` prefixes, `cache-hit` and the other outputs, `lookup-only` and `fail-on-cache-miss` work as on GitHub; a job restores caches saved on its branch, its pull request's base branch or the default branch, entries are never overwritten, and the oldest go once a repository's caches pass 10 GB. Caching that other actions do through GitHub's cache service (such as `setup-node`'s `cache:` input or `Swatinem/rust-cache`) isn't available locally, and those actions carry on without it. |

## The environment

Jobs run as `runner` with passwordless sudo, in `/home/runner/work/<repo>/<repo>`, with `RUNNER_OS=Linux`, `CI=true`, `GITHUB_ACTIONS=true` and the usual `GITHUB_*` and `RUNNER_*` variables. `BASTE=true` tells a step it runs locally, and `if: env.BASTE == 'true'` runs a step only locally (on GitHub, `env.BASTE` is empty). The image is a slim Ubuntu 24.04 with git, build tools, Python 3, Node.js, Docker and common CLI tools, not GitHub's full runner image. Tools that setup actions install (`actions/setup-node`, `setup-python`, ...) work as usual.

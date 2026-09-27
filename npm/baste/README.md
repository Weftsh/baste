# Baste

**Run your GitHub Actions locally. Merge on the green check.**

Baste runs the workflows you already have in a fresh Linux VM on your machine every time you `git push`, then posts each job to GitHub as a commit status that branch protection accepts. No queue, no workflow rewrites, no servers.

- **Website:** https://weftsh.github.io/baste/
- **Docs and source:** https://github.com/Weftsh/baste

## Install

```sh
npm install -g @weftsh/baste
```

This installs the native binary for your platform: macOS on Apple Silicon, Linux (x64, arm64), or Windows 11 inside WSL2. You also need the [GitHub CLI](https://cli.github.com), logged in with `gh auth login`.

Install globally rather than running `baste init` through `npx`: the git hook calls the installed binary on every push.

## Get started

```sh
cd your-repo
baste init          # checks virtualization, gh login and status permission, then installs a pre-push hook
git push            # returns right away; CI runs locally in the background
baste status        # each job's state
baste logs latest   # every step's output, live while it runs
```

Each local job shows up on the commit as `baste/<workflow>/<job>`. Make those required checks in branch protection, and pull requests merge on a local pass.

Your machine needs a VM backend set up once (Tart on macOS, KVM and Firecracker on Linux and WSL2). See [Get started](https://github.com/Weftsh/baste#get-started) for the steps.

Want to try it before anything reaches GitHub? `npx @weftsh/baste run --no-status` runs your workflows for `HEAD` locally and posts nothing.

## License

Apache-2.0

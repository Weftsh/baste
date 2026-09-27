#!/usr/bin/env bash
# Boot Baste's Firecracker guest path under QEMU's microvm machine, which
# needs no KVM (software emulation is slow but fine). It checks the parts that
# only run inside the VM:
#
#  1. a `prepare` boot: our initramfs /init, the overlay root, the hand-off to
#     systemd, the injected agent service reading its bundle from a raw block
#     device, running a provisioning script, and a clean reboot so the
#     resulting layer can be mounted read-only;
#  2. a `run` boot on top of that prepared layer (two read-only lowers plus a
#     scratch upper), running a job as the `runner` user.
#
# Events go to a virtio console here instead of vsock, which QEMU can't offer
# without /dev/vhost-vsock on the host. `tsc_early_khz` works around software
# emulation sometimes failing to calibrate the clock (KVM uses kvm-clock).
#
# Usage: scripts/qemu-smoke.sh <static linux baste> <vmlinux> <rootfs.squashfs>
set -euo pipefail

BASTE=$(realpath "$1")
KERNEL=$(realpath "$2")
ROOTFS=$(realpath "$3")
W=$(mktemp -d)
# KEEP=1 keeps the work directory (disks, console logs) for debugging.
[ -n "${KEEP:-}" ] && echo "Work directory: $W" || trap 'rm -rf "$W"' EXIT

BASTE_AGENT_BIN="$BASTE" BASTE_CACHE_DIR="$W/cache" "$BASTE" image initramfs --out "$W/initramfs.cpio" >/dev/null

make_tar() { # dir out
  tar -C "$1" -cf "$2" .
  local size
  size=$(stat -c %s "$2")
  truncate -s $(( (size + 511) / 512 * 512 + 4096 )) "$2"
}

boot() { # name mode args...
  local name=$1 mode=$2
  shift 2
  : > "$W/$name-events.log"
  echo "Booting '$name' under QEMU (software emulation)…"
  set +e
  timeout "${TIMEOUT:-900}" qemu-system-x86_64 \
    -M microvm,pit=on,pic=on,rtc=on -cpu max -m 1024 -smp 2 \
    -nodefaults -no-user-config -nographic -no-reboot \
    -serial "file:$W/$name-console.log" \
    -kernel "$KERNEL" -initrd "$W/initramfs.cpio" \
    -append "console=ttyS0 reboot=k panic=1 tsc_early_khz=2000000 ip=10.0.2.15::10.0.2.2:255.255.255.0::eth0:off baste.mode=$mode baste.events=/dev/hvc0 baste.dns=10.0.2.3 $BOOT_ARGS" \
    "$@" \
    -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
    -device virtio-serial-device -chardev "file,id=ev,path=$W/$name-events.log" -device virtconsole,chardev=ev
  local status=$?
  set -e
  echo "--- $name console (last 15 lines) ---"
  tail -n 15 "$W/$name-console.log" || true
  echo "--- $name events ---"
  cat "$W/$name-events.log"
  echo
  if [ "$status" -ne 0 ]; then
    echo "QEMU exited with $status" >&2
    exit 1
  fi
  grep -q '"type":"job_finished","result":"success"' "$W/$name-events.log" || { echo "$name did not succeed" >&2; exit 1; }
}

# 1. Prepare a layer.
mkdir -p "$W/prep-bundle"
cat > "$W/prep-bundle/provision.sh" <<'SH'
set -eux
useradd -m -u 1001 -s /bin/bash runner
mkdir -p /opt/baste /home/runner/work /opt/hostedtoolcache
echo provisioned > /opt/baste/marker
chown -R runner:runner /home/runner /opt/hostedtoolcache
echo "Provisioning finished"
SH
make_tar "$W/prep-bundle" "$W/prep-bundle.tar"
truncate -s 2G "$W/prepared.ext4"
mkfs.ext4 -q -F "$W/prepared.ext4"
BOOT_ARGS="baste.lower=/dev/vda baste.upper=/dev/vdb baste.bundle=/dev/vdc" boot prepare prepare \
  -drive "id=base,file=$ROOTFS,format=raw,if=none,readonly=on" -device virtio-blk-device,drive=base \
  -drive "id=upper,file=$W/prepared.ext4,format=raw,if=none" -device virtio-blk-device,drive=upper \
  -drive "id=bundle,file=$W/prep-bundle.tar,format=raw,if=none,readonly=on" -device virtio-blk-device,drive=bundle
grep -q 'Provisioning finished' "$W/prepare-events.log"
# The upper ext4 sits under the switched root, so it needs a journal replay
# (exit code 1 = "errors corrected"); Baste does the same on the host.
e2fsck -p -f "$W/prepared.ext4" >/dev/null || [ $? -le 2 ]

# 2. Run a job on top of it.
truncate -s 2G "$W/upper.ext4"
mkfs.ext4 -q -F "$W/upper.ext4"
mkdir -p "$W/bundle"
echo '{"ref":"refs/heads/main"}' > "$W/bundle/event.json"
cat > "$W/bundle/job.json" <<'JSON'
{
  "protocol": 1,
  "run_id": "smoke",
  "job_key": "smoke",
  "job_id": "smoke",
  "job_name": "smoke",
  "workflow": {"name": "Smoke", "file": ".github/workflows/smoke.yml"},
  "workflow_env": {"GREETING": "hello"},
  "steps": [
    {"id": "one", "name": "Kernel and user", "run": "echo \"$GREETING from $(uname -sm) as $(id -un)\"\necho \"root=$(findmnt -n -o FSTYPE /)\"\necho \"marker=$(cat /opt/baste/marker)\"\necho \"docker-dir=$(findmnt -n -o FSTYPE /var/lib/docker)\"\necho \"value=42\" >> \"$GITHUB_OUTPUT\""},
    {"name": "Outputs", "run": "echo \"got ${{ steps.one.outputs.value }}\"\ntest -x /usr/local/bin/baste\necho \"workspace=$GITHUB_WORKSPACE\""}
  ],
  "contexts": {"github": {"repository": "weftsh/baste", "event_name": "push"}, "matrix": {}, "strategy": {}, "needs": {}, "vars": {}, "inputs": {}},
  "secrets": {"GITHUB_TOKEN": "ghs_smoke"},
  "checkout": {"repository": "weftsh/baste", "sha": "0000000", "ref": "refs/heads/main", "packs": [], "server_url": "https://github.com"},
  "event_file": "event.json",
  "runner": {"os": "Linux", "arch": "X64", "name": "smoke", "work_root": "/home/runner/work", "tool_cache": "/opt/hostedtoolcache", "user": "runner"}
}
JSON
make_tar "$W/bundle" "$W/bundle.tar"
BOOT_ARGS="baste.lower=/dev/vda baste.prepared=/dev/vdb baste.upper=/dev/vdc baste.bundle=/dev/vdd" boot run run \
  -drive "id=base,file=$ROOTFS,format=raw,if=none,readonly=on" -device virtio-blk-device,drive=base \
  -drive "id=prepared,file=$W/prepared.ext4,format=raw,if=none,readonly=on" -device virtio-blk-device,drive=prepared \
  -drive "id=upper,file=$W/upper.ext4,format=raw,if=none" -device virtio-blk-device,drive=upper \
  -drive "id=bundle,file=$W/bundle.tar,format=raw,if=none,readonly=on" -device virtio-blk-device,drive=bundle

check() { grep -q "$1" "$W/run-events.log" || { echo "missing in run events: $1" >&2; exit 1; }; }
check 'hello from Linux x86_64 as runner'
check 'root=overlay'
check 'marker=provisioned'
check 'docker-dir=ext4'
check 'got 42'
check 'workspace=/home/runner/work/baste/baste'
echo "Smoke test passed"

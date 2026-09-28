#!/bin/bash
# Provision Baste's slim runner image: the tools GitHub's ubuntu runners have
# that most workflows rely on, a `runner` user, Node.js for JavaScript
# actions, and Docker. Runs once per pinned base image; the result is cached
# locally as a copy-on-write layer that every job VM boots from.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

ARCH=$(uname -m)
case "$ARCH" in
  x86_64) NODE_ARCH=x64 ;;
  aarch64) NODE_ARCH=arm64 ;;
  *) echo "unsupported architecture $ARCH" >&2; exit 1 ;;
esac

echo "::group::Packages"
for i in 1 2 3; do apt-get update && break || sleep 5; done
apt-get install -y --no-install-recommends \
  ca-certificates curl wget git git-lfs jq unzip zip xz-utils zstd bzip2 tar gzip \
  build-essential pkg-config python3 python3-pip python3-venv python-is-python3 \
  sudo openssh-client gnupg lsb-release software-properties-common rsync file time \
  locales tzdata iproute2 iptables docker.io containerd
# The VM kernel has iptables but not nftables.
update-alternatives --set iptables /usr/sbin/iptables-legacy || true
update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy || true
locale-gen en_US.UTF-8 >/dev/null || true
echo "::endgroup::"

echo "::group::Runner user"
if ! id runner >/dev/null 2>&1; then
  # GitHub's runner user has UID 1001, which some workflows rely on. Cirrus
  # Labs' Ubuntu image (Tart) gives 1001 to its unused stock `ubuntu` user,
  # so remove that one. Any other owner keeps 1001 and runner gets a free UID.
  owner=$(getent passwd 1001 | cut -d: -f1 || true)
  if [ "$owner" = ubuntu ]; then
    userdel -r ubuntu 2>/dev/null || userdel ubuntu
  fi
  if getent passwd 1001 >/dev/null; then
    useradd -m -s /bin/bash runner
  else
    useradd -m -u 1001 -s /bin/bash runner
  fi
fi
usermod -aG docker runner
echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner
chmod 0440 /etc/sudoers.d/runner
mkdir -p /home/runner/work /opt/hostedtoolcache
chown -R runner:runner /home/runner /opt/hostedtoolcache
echo "::endgroup::"

echo "::group::Node.js for JavaScript actions"
install_node() {
  local name=$1 version=$2 sha_x64=$3 sha_arm64=$4
  local sha=$sha_x64
  [ "$NODE_ARCH" = arm64 ] && sha=$sha_arm64
  local file="node-v${version}-linux-${NODE_ARCH}.tar.xz"
  curl -fsSLo "/tmp/$file" "https://nodejs.org/dist/v${version}/$file"
  echo "$sha  /tmp/$file" | sha256sum -c -
  mkdir -p "/opt/baste/$name"
  tar -xJf "/tmp/$file" -C "/opt/baste/$name" --strip-components=1
  rm -f "/tmp/$file"
}
install_node node20 20.20.2 \
  df770b2a6f130ed8627c9782c988fda9669fa23898329a61a871e32f965e007d \
  73093db209e4e9e09dd7d15a47aeaab1b74833830df03efa5f942a1122c5fa71
install_node node24 24.21.0 \
  fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6 \
  6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2
# `node` on PATH, like the runner images.
ln -sf /opt/baste/node24/bin/node /usr/local/bin/node
ln -sf /opt/baste/node24/bin/npm /usr/local/bin/npm
ln -sf /opt/baste/node24/bin/npx /usr/local/bin/npx
echo "::endgroup::"

if [ -n "${BASTE_ROSETTA:-}" ]; then
  echo "::group::Rosetta for x86_64 binaries"
  mkdir -p /media/rosetta
  cat > /etc/systemd/system/baste-rosetta.service <<'UNIT'
[Unit]
Description=Run x86_64 binaries through Rosetta for Linux
Before=docker.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'mountpoint -q /media/rosetta || mount -t virtiofs rosetta /media/rosetta; [ -e /proc/sys/fs/binfmt_misc/rosetta ] || echo ":rosetta:M::\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x3e\\x00:\\xff\\xff\\xff\\xff\\xff\\xfe\\xfe\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff:/media/rosetta/rosetta:CF" > /proc/sys/fs/binfmt_misc/register'
[Install]
WantedBy=multi-user.target
UNIT
  systemctl enable baste-rosetta.service
  echo "::endgroup::"
fi

# Docker 28+ filters with the iptables raw table; the Firecracker kernel has
# none (CONFIG_IP_NF_RAW), so skip those rules there. The VM sits behind NAT.
if ! iptables -t raw -L -n >/dev/null 2>&1; then
  mkdir -p /etc/systemd/system/docker.service.d
  printf '[Service]\nEnvironment=DOCKER_INSECURE_NO_IPTABLES_RAW=1\n' > /etc/systemd/system/docker.service.d/baste.conf
fi
systemctl enable docker.service containerd.service >/dev/null 2>&1 || true
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/*
echo "Provisioning finished"

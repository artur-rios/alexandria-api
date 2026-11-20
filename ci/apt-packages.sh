# shellcheck shell=bash
# Sourced by the scripts under ci/ that need system packages (not run on its own).
#
#     ensure_packages <package...>
#
# The GitHub job apt-installed ffmpeg's C development libraries on the runner; the pinned Rust image does not carry
# them. They are installed here with apt when one is missing and this runs as root, as checks do on yggdrasil's
# agents. As another user, with a package missing, it stops and says which (an image that already has them, or a
# machine of your own, needs no root).
ensure_packages() {
  local package missing=()
  for package in "$@"; do
    dpkg-query --show --showformat='${db:Status-Status}' "$package" 2>/dev/null | grep -qx installed \
      || missing+=("$package")
  done
  if [ "${#missing[@]}" -eq 0 ]; then
    return 0
  fi
  if [ "$(id -u)" -ne 0 ]; then
    echo "ci/apt-packages.sh: ${missing[*]} must be installed, and this is not run as root" >&2
    return 1
  fi
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends "${missing[@]}"
}

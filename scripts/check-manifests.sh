#!/usr/bin/env bash
# Keeps each example's deployment manifests honest.
#
# A workload manifest fails in ways that are invisible until you apply it: the
# published image tag drifts from the crate version, or MCP_ALLOWED_HOSTS stops
# matching the ingress host and every request comes back `Forbidden`. This
# script checks both, for every example, in CI and before a release.
#
#   ./scripts/check-manifests.sh                 # check all examples
#   ./scripts/check-manifests.sh fred-mcp        # check one
#
# Intentionally grep/sed only: the manifests are small and fixed in shape, and
# a YAML library is one more thing to install on a runner.
set -u
cd "$(dirname "$0")/.." || exit 1

# Packages live under the repository's own namespace: the org's top level is
# shared, and `sec-edgar-mcp` there already belongs to another repo.
REGISTRY="ghcr.io/cosmonic-labs/mcp-examples"
LOCAL_REGISTRY="oci-registry.localhost:8200"

FAIL=0
ok() { echo "  ok   - $1"; }
bad() {
  FAIL=$((FAIL + 1))
  echo "  FAIL - $1"
  echo "         ${2:-}"
}

# yaml_value <file> <key> — the first `key: value` in the file, unquoted.
yaml_value() {
  sed -n "s/^[[:space:]]*${2}:[[:space:]]*//p" "$1" | head -1 | sed 's/^"//; s/"$//'
}

# yaml_values <file> <key> — every `key: value` in the file, unquoted.
yaml_values() {
  sed -n "s/^[[:space:]]*${2}:[[:space:]]*//p" "$1" | sed 's/^"//; s/"$//'
}

check_package() {
  local pkg="$1"
  echo "== $pkg =="

  local cargo_version
  cargo_version=$(sed -n '/^\[package\]/,/^\[/ s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' \
    "$pkg/Cargo.toml" | head -1)
  if [ -z "$cargo_version" ]; then
    bad "$pkg: read version from Cargo.toml" "no [package] version found"
    return
  fi
  ok "Cargo.toml version is $cargo_version"

  # --- deploy/workload.yaml: the published image ----------------------------
  local deploy="$pkg/deploy/workload.yaml"
  if [ ! -f "$deploy" ]; then
    bad "$pkg: deploy/workload.yaml exists" "missing $deploy"
    return
  fi

  local want_image="${REGISTRY}/${pkg}:${cargo_version}"
  local got_image
  got_image=$(yaml_value "$deploy" image)
  if [ "$got_image" = "$want_image" ]; then
    ok "deploy image is $want_image"
  else
    bad "deploy image matches the crate version" \
      "want $want_image, got ${got_image:-<none>}"
  fi

  # --- workload.yaml: the local promote loop --------------------------------
  local local_manifest="$pkg/workload.yaml"
  local got_local
  got_local=$(yaml_value "$local_manifest" image)
  case "$got_local" in
    "${LOCAL_REGISTRY}/${pkg}:"*) ok "local image is $got_local" ;;
    *) bad "local workload.yaml points at Desktop's registry" \
      "want ${LOCAL_REGISTRY}/${pkg}:<version>, got ${got_local:-<none>}" ;;
  esac

  # --- version labels -------------------------------------------------------
  local manifest label_version
  for manifest in "$deploy" "$local_manifest"; do
    label_version=$(sed -n 's/^[[:space:]]*app.kubernetes.io\/version:[[:space:]]*//p' \
      "$manifest" | head -1 | sed 's/^"//; s/"$//')
    if [ "$label_version" = "$cargo_version" ]; then
      ok "$manifest app.kubernetes.io/version is $cargo_version"
    else
      bad "$manifest app.kubernetes.io/version matches the crate version" \
        "want $cargo_version, got ${label_version:-<none>}"
    fi
  done

  # --- ingress host vs the DNS-rebinding guard ------------------------------
  # Cosmonic routes to a workload by Host header, and the transport rejects any
  # Host not in MCP_ALLOWED_HOSTS. Every ingress host must therefore be listed,
  # or the deployment answers `Forbidden` and nothing else.
  local hosts allowed host
  for manifest in "$deploy" "$local_manifest"; do
    hosts=$(yaml_values "$manifest" host)
    allowed=$(sed -n 's/^[[:space:]]*MCP_ALLOWED_HOSTS:[[:space:]]*//p' "$manifest" |
      head -1 | sed 's/^"//; s/"$//')
    if [ -z "$hosts" ]; then
      bad "$manifest declares an ingress host" "no hostInterfaces host found"
      continue
    fi
    for host in $hosts; do
      case ",${allowed}," in
        *",${host},"*) ok "$manifest allows ingress host $host" ;;
        *) bad "$manifest lists $host in MCP_ALLOWED_HOSTS" \
          "MCP_ALLOWED_HOSTS=${allowed:-<unset>} — requests would be rejected as Forbidden" ;;
      esac
    done
  done
}

PACKAGES=("$@")
if [ ${#PACKAGES[@]} -eq 0 ]; then
  PACKAGES=(after-effects-mcp cfpb-complaints-mcp fred-mcp premiere-mcp sec-edgar-mcp)
fi

for pkg in "${PACKAGES[@]}"; do
  check_package "$pkg"
done

echo
if [ "$FAIL" -eq 0 ]; then
  echo "== manifests ok =="
else
  echo "== $FAIL manifest problem(s) =="
fi
exit $((FAIL > 0))

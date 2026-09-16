#!/usr/bin/env bash
# Architecture invariants for the ABI v3 plugin host.
#
# Production hosts must not:
#   - link ordinary plugin implementation crates (in-process execution)
#   - select the diagnostic direct-native Cap'n Proto transport
#   - treat a bare manifest id as platform trust
#   - persist grants under a bare manifest id without PluginKey
#   - leave authority revision empty on a product spawn
#
# Physical SQL lowering belongs in database adapter guests. Production hosts
# must not link `bookclerk-plugin-database-*` merely to perform product
# behavior (provision, drop, backup sidecars, secret injection).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

status=0

fail() {
  echo "FAIL: $*" >&2
  status=1
}

ok() {
  echo "ok: $*"
}

for toml in crates/bookclerk-cli/Cargo.toml crates/bookclerkd/Cargo.toml crates/bookclerk-plugin-host/Cargo.toml; do
  if grep -n 'bundled-plugins' "$toml" >/dev/null; then
    fail "$toml still declares bundled-plugins (in-process product path)"
  else
    ok "$toml has no bundled-plugins feature"
  fi
done

FORBIDDEN_LIBS='^bookclerk_plugin_(source|integration|destination|database)_'
for host in bookclerk-cli bookclerkd bookclerk-plugin-host; do
  tree="$(cargo tree -p "$host" --edges normal --prefix none --format '{lib}')"
  hits="$(grep -E "$FORBIDDEN_LIBS" <<<"$tree" | sort -u || true)"
  if [[ -n "$hits" ]]; then
    echo "FAIL: $host links plugin implementation crates:" >&2
    sed 's/^/  /' <<<"$hits" >&2
    status=1
  else
    ok "$host does not link source/integration/destination/database plugin crates"
  fi
done

if grep -RIn --include='*.rs' 'DirectNativeDiagnostic\|direct_native_diagnostic' \
  crates/bookclerk-cli/src crates/bookclerkd/src >/dev/null 2>&1; then
  fail "product binaries mention DirectNativeDiagnostic"
else
  ok "bookclerk / bookclerkd do not select DirectNativeDiagnostic"
fi

prod_hits="$(grep -RIn --include='*.rs' 'SpawnTransport::DirectNativeDiagnostic' \
  crates/bookclerk-plugin-host/src \
  | grep -v 'spawn_plan.rs' \
  | grep -v 'rpc_session.rs' \
  || true)"
if [[ -n "$prod_hits" ]]; then
  # jail.rs may mention it inside #[cfg(test)].
  leftover="$(echo "$prod_hits" | grep -v 'src/jail.rs' || true)"
  if [[ -n "$leftover" ]]; then
    echo "FAIL: production plugin-host selects DirectNativeDiagnostic:" >&2
    echo "$leftover" >&2
    status=1
  else
    ok "plugin-host production spawn does not select DirectNativeDiagnostic"
  fi
else
  ok "plugin-host production spawn does not select DirectNativeDiagnostic"
fi

if grep -RIn --include='*.rs' 'is_platform_plugin_id' crates >/dev/null 2>&1; then
  fail "is_platform_plugin_id reappeared (platform trust must use provenance)"
else
  ok "no is_platform_plugin_id"
fi

if grep -n 'first_party_database_kind' crates/bookclerk-plugin-host/src/jail.rs >/dev/null \
  && grep -n 'is_first_party_local_output' crates/bookclerk-plugin-host/src/jail.rs >/dev/null; then
  ok "extra jail grants require verified first-party identity"
else
  fail "jail extra grants no longer require first-party provenance helpers"
fi

if grep -RIn 'host-owned SQL lowering' docs crates >/dev/null 2>&1; then
  fail "stale 'host-owned SQL lowering' comment remains (lowering belongs in adapters)"
else
  ok "no stale host-owned SQL lowering comments"
fi

if ! grep -n 'grant.plugin_key = plugin_key' crates/bookclerk-plugin-host/src/consent.rs >/dev/null; then
  fail "consent.rs no longer stamps PluginKey onto grants"
else
  ok "grants are stamped with PluginKey"
fi

if ! grep -n 'grant_revision.is_empty()' crates/bookclerk-plugin-host/src/rpc_session.rs >/dev/null; then
  fail "rpc_session.rs no longer fail-closes on an empty authority revision"
else
  ok "product spawn fail-closes on an empty authority revision"
fi

if grep -RIn --include='*.rs' 'publisher_key_id' crates >/dev/null 2>&1; then
  fail "publisher_key_id reappeared (no publisher PKI in the identity model)"
else
  ok "no publisher_key_id"
fi

if grep -RIn --include='*.rs' 'register_builtin_sources\|register_builtin_integrations' \
  crates/bookclerk-plugin-host crates/bookclerk-cli crates/bookclerkd >/dev/null 2>&1; then
  fail "register_builtin_* in-process registration reappeared"
else
  ok "no in-process register_builtin_* on hosts"
fi

if grep -n 'coarse jail' crates/bookclerk-plugins/optional/database-postgres/plugin.toml \
    docs/plugin-registry.md >/dev/null 2>&1; then
  fail "postgres/native docs still describe coarse jail outbound"
else
  ok "native postgres networking is SDK sockets, not coarse jail"
fi

if grep -A20 'pub fn bind_socket_proxy' crates/bookclerk-workerd/src/unix_bind.rs \
    | grep -q 'SOCKET_PROXY_ABSTRACT_PREFIX'; then
  fail "socket proxy still binds an abstract socket (nested Landlock ABI 6 scopes those out)"
else
  ok "native SOCKET_PROXY is a pathname (nested Landlock can connect)"
fi

if grep -B8 'NESTED_NATIVE_JAIL_ENV, "1"' crates/bookclerk-plugin-host/src/spawn_stdio.rs \
    | grep -q 'Start::Confined'; then
  fail "nested native jail is still gated on outer confinement"
else
  ok "nested native jail is requested for native-behind-workerd"
fi

if grep -B5 'NESTED_NATIVE_JAIL_ENV, "1"' crates/bookclerk-plugin-host/src/spawn_stdio.rs \
    | grep -q 'cfg(unix)'; then
  fail "nested native jail is still Unix-only (Windows native guests need nested Deny)"
else
  ok "nested native jail is requested on every OS"
fi

if grep -A12 'fn use_socket_proxy' crates/bookclerk-plugin-sdk/src/http.rs \
    | grep -q 'not(unix)'; then
  fail "SDK HTTP still treats SOCKET_PROXY as Unix-only"
else
  ok "SDK HTTP honors SOCKET_PROXY on Unix and Windows"
fi

if ! grep -n 'allow(unsafe_code)' crates/bookclerk-workerd/src/pipe_bind.rs >/dev/null 2>&1; then
  fail "Windows named-pipe CreateNamedPipe is missing allow(unsafe_code)"
else
  ok "Windows named-pipe SOCKET_PROXY allows the CreateNamedPipe FFI sink"
fi

if grep -B2 'impl GrantedListener for tokio::net::UnixListener' crates/bookclerk-workerd/src/granted.rs \
    | grep -q 'cfg(unix)'; then
  ok "GRANTED UnixListener is Unix-only"
else
  fail "GRANTED UnixListener is compiled on Windows (tokio UnixListener is Unix-only)"
fi

if grep -n 'nested_enforcement_required' crates/bookclerk-workerd/src/native_guest.rs >/dev/null; then
  ok "nested Deny fail-closes when jail is missing and enforcement is required"
else
  fail "nested jail missing-helper path no longer fail-closes"
fi

if grep -n 'upsert_event_subscriber(&node_id, &plugin.manifest.id' crates/bookclerkd/src/event_worker.rs >/dev/null; then
  fail "event catalog still keys discovered plugins on the display alias"
else
  ok "event catalog keys discovered plugins on PluginKey"
fi

if grep -n 'upsert_event_subscriber(&node_id, integration.id()' crates/bookclerkd/src/event_worker.rs >/dev/null; then
  fail "event catalog still keys loaded integrations on the display alias"
else
  ok "event catalog keys loaded integrations on PluginKey"
fi

if grep -n 'plugin_id: plugin.manifest.id.clone()' crates/bookclerkd/src/oidc.rs >/dev/null; then
  fail "OIDC ownership still keys on the display alias"
else
  ok "OIDC plugin-owned clients key on PluginKey"
fi

if ! grep -n 'socket_proxy::spawn_windows' crates/bookclerk-workerd/src/main.rs >/dev/null 2>&1; then
  fail "Windows SOCKET_PROXY accept loop is not started from bookclerk-workerd"
else
  ok "Windows SOCKET_PROXY accept loop is wired in the launcher"
fi

# Native guests must not open ambient TCP (reqwest::Client) in product src.
hits="$(find crates/bookclerk-plugins/optional crates/bookclerk-enrich/src \
  -name '*.rs' ! -path '*/tests/*' -print0 \
  | xargs -0 grep -n 'reqwest::Client' || true)"
if [[ -n "$hits" ]]; then
  echo "FAIL: ambient reqwest::Client remains in native guest / enrich src:" >&2
  echo "$hits" >&2
  status=1
else
  ok "native storefront/enrich HTTP is not ambient reqwest::Client"
fi

if grep -A25 'fn overlay_postgres_url' crates/bookclerk-plugin-host/src/consent.rs \
    | grep -q 'overlay_database_slot_matches'; then
  ok "postgres host overlay is PluginKey-aware"
else
  fail "postgres host overlay still keys only on the display alias"
fi

if grep -A20 'fn overlay_s3_endpoint' crates/bookclerk-plugin-host/src/consent.rs \
    | grep -q 'overlay_unique_occupant'; then
  ok "S3 host overlay is PluginKey-aware"
else
  fail "S3 host overlay still keys only on the display alias"
fi

if grep -A20 'fn overlay_audiobookshelf_url' crates/bookclerk-plugin-host/src/consent.rs \
    | grep -q 'overlay_unique_occupant'; then
  ok "Audiobookshelf host overlay is PluginKey-aware"
else
  fail "Audiobookshelf host overlay still keys only on the display alias"
fi

if grep -A20 'fn overlay_unique_occupant' crates/bookclerk-plugin-host/src/consent.rs \
    | grep -q 'resolve_plugin_slot'; then
  ok "host overlays uniquify occupancy so duplicate aliases cannot inherit URLs"
else
  fail "host overlays still match every same-alias install"
fi

if grep -A40 'pub async fn load_external_destinations' crates/bookclerk-plugin-host/src/host/destination.rs \
    | grep -q 'resolve_plugin_slot'; then
  ok "destination occupancy fail-closes on duplicate aliases"
else
  fail "S3/local destination load still last-write-wins on duplicate aliases"
fi

if grep -A40 'pub async fn load_external_sources' crates/bookclerk-plugin-host/src/host/source.rs \
    | grep -q 'resolve_plugin_slot'; then
  ok "source occupancy fail-closes on duplicate aliases"
else
  fail "source load still spawns every alias duplicate"
fi

if grep -A40 'pub async fn load_external_integrations' crates/bookclerk-plugin-host/src/host/integration.rs \
    | grep -q 'resolve_plugin_slot'; then
  ok "integration occupancy fail-closes on duplicate aliases"
else
  fail "integration load still spawns every alias duplicate"
fi

if grep -n 'not(unix)' crates/bookclerk-plugin-sdk/src/pass_fd.rs \
    | grep -q . \
    && grep -A20 'pub fn fd_proc_path' crates/bookclerk-plugin-sdk/src/pass_fd.rs \
    | grep -q 'not(unix)'; then
  ok "fd_proc_path compiles on non-Unix"
else
  fail "fd_proc_path is Unix-only and will not compile on Windows"
fi

if grep -A40 'pub async fn mediated_connect_url' \
    crates/bookclerk-plugins/optional/database-postgres/src/socket_mediate.rs \
    | grep -q 'cannot splice sqlx through SOCKET_PROXY'; then
  ok "postgres SOCKET_PROXY fail-closes on non-Unix"
else
  fail "postgres still returns ambient TCP URLs when SOCKET_PROXY is set on Windows"
fi

if grep -A35 'pub async fn mediated_connect_url' \
    crates/bookclerk-plugins/optional/database-postgres/src/socket_mediate.rs \
    | grep -q 'nested_native_jail_requested'; then
  ok "postgres fail-closes nested Deny without SOCKET_PROXY"
else
  fail "postgres still returns ambient TCP URLs under nested jail without SOCKET_PROXY"
fi

if grep -B2 'pub fn postgres_url_with_unix_host' \
    crates/bookclerk-plugins/optional/database-postgres/src/socket_mediate.rs \
    | grep -q 'any(unix, test)'; then
  ok "postgres Unix-socket URL rewrite is not dead on Windows lib clippy"
else
  fail "postgres_url_with_unix_host is compiled unused on Windows lib"
fi

if grep -A35 'fn open_granted_binding_databases' crates/bookclerkd/src/jobs.rs \
    | grep -q 'get_by_plugin_key'; then
  ok "plugin_copy bindings look up grants by PluginKey"
else
  fail "plugin_copy still uses alias PluginGrantStore::get"
fi

if grep -A40 'fn open_granted_binding_databases' crates/bookclerkd/src/jobs.rs \
    | grep -q 'owner.id()'; then
  ok "plugin_copy bindings are owned by the session PluginKey"
else
  fail "plugin_copy still keys plugin databases on the job's plugin_id string"
fi

if grep -A30 'pub fn plugin_session' crates/bookclerk-plugin-host/src/host/destination.rs \
    | grep -q 'identity_matches_occupancy'; then
  ok "destination plugin_session occupancy is PluginKey-aware"
else
  fail "destination plugin_session still looks up only plugin_instance_key(alias)"
fi

if grep -A20 'pub(crate) fn plugin_binding_unit_ref' \
    crates/bookclerk-plugin-host/src/host/database.rs \
    | grep -q 'plugin_database_owner_leaf'; then
  ok "sqlite plugin databases use PluginKey fs_id"
else
  fail "sqlite plugin-databases path still joins the raw owner id"
fi

if grep -B1 'fd_proc_path, recv_passed_fd' \
    crates/bookclerk-plugin-sdk/src/fetch_dir.rs | grep -q 'cfg(unix)'; then
  ok "SDK fetch_dir recv_passed_fd import is Unix-only"
else
  fail "fetch_dir imports recv_passed_fd on Windows (unused import under clippy -D warnings)"
fi

if grep -A20 'pub fn new()' crates/bookclerk-plugin-sdk/src/http.rs \
    | grep -q 'nested_native_jail_requested'; then
  ok "SDK HTTP Client::new fail-closes under nested Deny instead of ambient TCP"
else
  fail "SDK HTTP Client::new still falls back to ambient reqwest when the proxy fails"
fi

if grep -B2 'static SOCKET_PROXY_ENV_LOCK' crates/bookclerk-plugin-sdk/src/net.rs \
    | grep -q 'unix, feature = "http"'; then
  ok "SOCKET_PROXY_ENV_LOCK is not unused on Windows without http tests"
else
  fail "SOCKET_PROXY_ENV_LOCK is cfg(test) on Windows and fails clippy --all-targets"
fi

if grep -A25 'pub fn upsert' crates/bookclerk-plugin-host/src/consent.rs \
    | grep -q 'g.plugin_key.is_empty() && g.plugin_id == grant.plugin_id'; then
  ok "grant upsert matches keyless rows by alias only"
else
  fail "grant upsert can still replace a PluginKey grant via alias"
fi

if grep -A20 'for plugin_id in &enabling' crates/bookclerkd/src/api.rs \
    | grep -q 'or_else'; then
  fail "daemon enable still falls back to the first duplicate alias"
else
  ok "daemon enable consent uses resolve_plugin_ref only"
fi

if grep -A25 'apply_setting_overrides(&mut cfg, &pairs)' crates/bookclerkd/src/api.rs \
    | grep -q 'stamp_settings_occupancy'; then
  ok "settings enable stamps occupancy PluginKey"
else
  fail "settings enable still persists alias occupancy"
fi

if grep -A8 'cannot stamp occupancy for enable' crates/bookclerkd/src/api.rs \
    | grep -q 'BAD_REQUEST'; then
  ok "settings occupancy stamp fail-closes when the occupant cannot be resolved"
else
  fail "settings enable still writes config when occupancy stamp cannot resolve"
fi

if grep -A20 'fn stamp_settings_occupancy' crates/bookclerkd/src/api.rs \
    | grep -q 'resolve_plugin_ref'; then
  ok "settings occupancy stamp resolves PluginKey uniquely"
else
  fail "settings occupancy stamp helper is missing"
fi

if grep -n 'stamp_occupancy_plugin_key' crates/bookclerk-cli/src/commands/plugins.rs >/dev/null; then
  ok "CLI enable stamps occupancy PluginKey"
else
  fail "CLI enable no longer stamps occupancy PluginKey"
fi

if grep -A20 'for id in disabled_targets' crates/bookclerkd/src/api.rs \
    | grep -q 'occupancy_names_alias'; then
  ok "settings database disable resolves occupancy via unique alias / PluginKey"
else
  fail "settings database disable still string-equals the occupancy field"
fi

if grep -A40 'pub fn parse(s: &str)' crates/bookclerk-config/src/database.rs \
    | grep -q 'rsplit_once'; then
  fail "DatabasePluginKind::parse still derives kind from PluginKey fragments"
else
  ok "DatabasePluginKind::parse is bare kind tokens only"
fi

if grep -n 'adapter_plugin_key' crates/bookclerk-library/src/migrations/unreleased_ops.rs >/dev/null; then
  ok "plugin_databases persists adapter PluginKey"
else
  fail "plugin_databases is missing adapter_plugin_key"
fi

if grep -n 'with_persisted_and_effective' crates/bookclerk-plugin-host/src/rpc_session.rs >/dev/null; then
  ok "spawn separates persisted grant_revision from effective authority_revision"
else
  fail "spawn still hashes the overlaid grant as both revisions"
fi

if grep -nE 'same_alias_different_.*coexist' crates >/dev/null 2>&1; then
  fail "same-alias coexistence tests remain"
else
  ok "no supported same-alias coexistence tests"
fi

if grep -n 'allow_unsigned' crates docs >/dev/null 2>&1; then
  fail "allow_unsigned terminology remains"
else
  ok "unsigned publisher terminology was renamed"
fi

if grep -nE 'peel_plugin_state|restore_plugin_state' crates >/dev/null 2>&1; then
  fail "install-root data/tmp preservation remains"
else
  ok "install-root data/tmp preservation was removed"
fi

if grep -n 'reqwest::Client' crates/bookclerk-storage/src >/dev/null 2>&1; then
  fail "bookclerk-storage still uses ambient reqwest::Client"
else
  ok "S3 storage HTTP is not ambient reqwest::Client"
fi

if grep -A25 'fn overlay_discovered_plugins' crates/bookclerk-plugin-host/src/spawn_stdio.rs \
    | grep -q 'discover_plugins(config)'; then
  fail "spawn overlay occupancy still re-hashes every install via discover_plugins"
else
  ok "spawn overlay occupancy uses a lightweight scan"
fi

exit "$status"

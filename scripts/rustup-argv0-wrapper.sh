#!/bin/sh
# Cursor's AppImage/sandbox often execs children with argv0 "cursor".
# rustup selects its proxy tool from argv0, so bare ~/.cargo/bin/cargo fails with:
#   error: unknown proxy name: 'cursor'
# Re-exec the real rustup binary with the correct argv0 (this script's name).
#
# Install on the host (once), ahead of ~/.cargo/bin on PATH:
#   ln -sfn "$PWD/scripts/rustup-argv0-wrapper.sh" ~/.local/bin/rustup-argv0-wrapper
#   for name in cargo rustc rustup rustdoc rustfmt clippy-driver cargo-clippy cargo-fmt rust-analyzer; do
#     ln -sfn rustup-argv0-wrapper ~/.local/bin/"$name"
#   done
#
# Cursor interpolates ${workspaceFolder} in terminal.integrated.env as ~/...
# Cargo and autotools do not expand tilde, so CARGO_TARGET_DIR becomes
# $PWD/~/Projects/.../target and config.guess cannot create files in TMPDIR.
# Expand those env vars here before rustup/cargo see them.
#
# If CARGO_HOME is overridden (Dev Container / Cloud bind-mount cache), it is a
# registry/git cache and does not contain rustup. Always prefer
# $HOME/.cargo/bin/rustup and never re-exec this wrapper.

name=${0##*/}

expand_tilde() {
  # Do not use ${1#~/}: unquoted ~ in a ${#} pattern is tilde-expanded to
  # $HOME/, so ~/Projects/... becomes $HOME/~/Projects/... instead of
  # $HOME/Projects/....
  case $1 in
    '~/'*) printf '%s' "$HOME/${1#??}" ;;
    '~') printf '%s' "$HOME" ;;
    *) printf '%s' "$1" ;;
  esac
}

expand_path_list() {
  _in=$1
  _out=
  _oldifs=$IFS
  IFS=:
  # shellcheck disable=SC2086
  set -- ${_in}
  IFS=${_oldifs}
  for _p in "$@"; do
    _ep=$(expand_tilde "${_p}")
    if [ -z "${_out}" ]; then
      _out=${_ep}
    else
      _out="${_out}:${_ep}"
    fi
  done
  printf '%s' "${_out}"
  unset _in _out _oldifs _p _ep
}

for _var in CARGO_HOME CARGO_TARGET_DIR TMPDIR BOOKCLERK_FILES_DIR RUSTUP_HOME; do
  eval "_val=\${${_var}-}"
  if [ -n "${_val}" ]; then
    _exp=$(expand_tilde "${_val}")
    eval "export ${_var}=\"\${_exp}\""
  fi
done
unset _var _val _exp

if [ -n "${PATH-}" ]; then
  PATH=$(expand_path_list "${PATH}")
  export PATH
fi

resolve() {
  if command -v realpath >/dev/null 2>&1; then
    realpath "$1" 2>/dev/null || printf '%s' "$1"
  elif readlink -f "$1" >/dev/null 2>&1; then
    readlink -f "$1"
  else
    printf '%s' "$1"
  fi
}

self=$(resolve "$0")

is_real_rustup() {
  [ -n "$1" ] && [ -x "$1" ] || return 1
  _res=$(resolve "$1")
  [ "${_res}" != "${self}" ]
}

real_rustup=
for _c in \
  "${HOME}/.cargo/bin/rustup" \
  "${CARGO_HOME:+${CARGO_HOME}/bin/rustup}"; do
  if is_real_rustup "${_c}"; then
    real_rustup=${_c}
    break
  fi
done
unset _c

if [ -z "${real_rustup}" ]; then
  echo "rustup-argv0-wrapper: missing rustup (tried \$HOME/.cargo/bin/rustup)" >&2
  echo "rustup-argv0-wrapper: CARGO_HOME may be a registry cache, not a rustup install" >&2
  exit 127
fi

exec -a "${name}" "${real_rustup}" "$@"

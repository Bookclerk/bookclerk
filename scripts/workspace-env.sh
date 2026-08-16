# Bookclerk workspace Cargo + data dirs.
#
# Source this file (do not execute):
#   source scripts/workspace-env.sh
#
# direnv: copy `.envrc.example` to `.envrc` (gitignored) and `direnv allow`.
# Cursor/VS Code terminals also get these via `.vscode/settings.json`.
#
# Puts `target/debug` and `target/release` on PATH so host binaries can be
# invoked directly (avoids `cargo run` argv0 issues in Cursor).

if [ -n "${BASH_SOURCE[0]:-}" ]; then
  _bookclerk_env_src="${BASH_SOURCE[0]}"
elif [ -n "${ZSH_VERSION:-}" ]; then
  # zsh: this file when sourced
  # shellcheck disable=SC2296
  _bookclerk_env_src="${(%):-%x}"
else
  _bookclerk_env_src="$0"
fi

_bookclerk_root="$(CDPATH= cd -- "$(dirname -- "${_bookclerk_env_src}")/.." && pwd)"
unset _bookclerk_env_src

mkdir -p \
  "${_bookclerk_root}/.cargo-home" \
  "${_bookclerk_root}/.tmp" \
  "${_bookclerk_root}/BookclerkFiles" \
  "${_bookclerk_root}/target"

export CARGO_HOME="${_bookclerk_root}/.cargo-home"
export CARGO_TARGET_DIR="${_bookclerk_root}/target"
export TMPDIR="${_bookclerk_root}/.tmp"
export BOOKCLERK_FILES_DIR="${_bookclerk_root}/BookclerkFiles"

case ":${PATH}:" in
  *":${_bookclerk_root}/target/debug:"*) ;;
  *)
    PATH="${_bookclerk_root}/target/debug:${_bookclerk_root}/target/release:${PATH}"
    export PATH
    ;;
esac

unset _bookclerk_root

#!/usr/bin/env bash
# Reject system-reserved secret names in GitHub Actions workflow files.
set -euo pipefail

if (($# > 0)); then
  files=("$@")
else
  if ! command -v git >/dev/null 2>&1; then
    echo "error: git is not on PATH. Install git first." >&2
    exit 1
  fi

  cd "$(git rev-parse --show-toplevel)"

  mapfile -d '' -t files < <(
    git ls-files -z --cached --others --exclude-standard -- \
      '.github/workflows/*.yml' '.github/workflows/*.yaml'
  )
fi

if ((${#files[@]} == 0)); then
  exit 0
fi

awk '
function trim(text) {
  sub(/^[[:space:]]+/, "", text)
  sub(/[[:space:]]+$/, "", text)
  return text
}

function unquote(text, quote) {
  quote = sprintf("%c", 39)
  gsub(/"/, "", text)
  gsub(quote, "", text)
  return text
}

function report(number, name) {
  printf "%s:%d: error: GitHub reserves the secret name %s. Remove the name from the secrets mapping.\n", FILENAME, number, name
  failed = 1
}

function check_flow(text, number,    i, name, pattern) {
  for (i = 1; i <= 3; i++) {
    name = names[i]
    pattern = "(^|[{,])[[:space:]]*" name "[[:space:]]*:"
    if (text ~ pattern) {
      report(number, name)
    }
  }
}

BEGIN {
  names[1] = "GITHUB_TOKEN"
  names[2] = "ACTIONS_STEP_DEBUG"
  names[3] = "ACTIONS_RUNNER_DEBUG"
  secrets_indent = -1
  block_indent = -1
  failed = 0
}

{
  text = $0
  sub(/[[:space:]]#.*$/, "", text)

  if (text ~ /^[[:space:]]*$/) {
    next
  }

  match(text, /^[[:space:]]*/)
  indent = RLENGTH

  if (block_indent >= 0) {
    if (indent > block_indent) {
      next
    }
    block_indent = -1
  }

  # Skip block scalar content. The check does not parse YAML values.
  if (text ~ /:[[:space:]]*[|>][+-]?[0-9]?[[:space:]]*$/) {
    block_indent = indent
    next
  }

  content = substr(text, indent + 1)
  if (substr(content, 1, 1) == "-") {
    next
  }

  colon = index(content, ":")
  if (colon == 0) {
    next
  }

  key = trim(substr(content, 1, colon - 1))
  value = substr(content, colon + 1)

  if (value != "" && substr(value, 1, 1) ~ /[^[:space:]]/) {
    next
  }

  key = unquote(key)
  if (key == "") {
    next
  }

  if (secrets_indent >= 0 && indent <= secrets_indent) {
    secrets_indent = -1
  }

  value = trim(value)

  if (key == "secrets") {
    if (value ~ /^\{/) {
      check_flow(unquote(value), FNR)
    } else if (value == "") {
      secrets_indent = indent
    }
    next
  }

  if (secrets_indent >= 0 && indent > secrets_indent) {
    for (i = 1; i <= 3; i++) {
      if (key == names[i]) {
        report(FNR, key)
      }
    }
  }
}

END {
  exit failed
}
' "${files[@]}"

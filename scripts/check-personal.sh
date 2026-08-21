#!/usr/bin/env bash
# Pre-commit guard: fail when staged files contain personal details —
# home-directory paths, hostname-derived emails/domains, or machine names.
# Secrets are detect-secrets' job; this hook is only about personal leakage.
set -uo pipefail

# Generic patterns; nothing user-specific is hardcoded here.
patterns=(
  '/Users/'
  '/home/[a-z0-9_-]+'
  '@[A-Za-z0-9.-]+\.local\b'
  '\.local/|\.home/'
  'Vishnus-MacBook'
)

files=$(git diff --cached --name-only --diff-filter=ACM)
found=0
for f in $files; do
  # The detector's own pattern literals would match themselves, and ci.yml
  # embeds the same literals for CI-side scanning.
  case "$f" in
  scripts/check-personal.sh | .github/workflows/ci.yml) continue ;;
  esac
  [ -f "$f" ] || continue
  for pat in "${patterns[@]}"; do
    matches=$(git diff --cached -- "$f" | grep -E '^\+' | grep -vE '^\+\+\+' | grep -inE "$pat") || continue
    echo "personal detail in $f (pattern: $pat):" >&2
    echo "$matches" | head -5 >&2
    found=1
  done
done

if [ "$found" -eq 1 ]; then
  echo "commit blocked: strip personal details from staged files" >&2
  exit 1
fi

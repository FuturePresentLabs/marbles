#!/usr/bin/env bash
# import-repo.sh — migrate one Beads store (a `bd export` JSONL) into its own marbles project.
#
#   scripts/import-repo.sh [marbles flags…] <project-slug> <beads-export.jsonl>
#
# Examples:
#   scripts/import-repo.sh myrepo myrepo.jsonl
#   scripts/import-repo.sh --prefix ppro --rewrite-prefix app:ppro ppro app.jsonl
#   MARBLES_HOME=/tmp/db scripts/import-repo.sh --dry-run myrepo myrepo.jsonl
#
# Flags before the slug are passed through to `marbles import-bd` verbatim
# (--prefix, --rewrite-prefix, --dry-run, --root); nothing here invents policy.
# When no --prefix is given, the store's own dominant id prefix is derived
# from the file, so new ids minted later match the imported ones.
#
# The import is safe to re-run: rows already in place from the same export are
# counted `unchanged`, and an id that exists with *different* content aborts
# before writing anything. `conflicts` in the report is the only failure worth
# reading closely; everything else the importer found ambiguous was reported
# and kept, not guessed.
set -euo pipefail

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

slug=""
file=""
extra=()
args=("$@")
while [[ ${#args[@]} -gt 0 ]]; do
  arg=${args[0]}
  args=("${args[@]:1}")
  case "$arg" in
    -h|--help) usage ;;
    --dry-run) extra+=("$arg") ;;
    --prefix|--rewrite-prefix|--root|--project)
      [[ ${#args[@]} -ge 1 ]] || { echo "$arg needs a value" >&2; exit 2; }
      extra+=("$arg" "${args[0]}")
      args=("${args[@]:1}")
      ;;
    --*=*) extra+=("$arg") ;;
    *)
      if [[ -z $slug ]]; then slug=$arg
      elif [[ -z $file ]]; then file=$arg
      else echo "unexpected argument: $arg" >&2; usage
      fi
      ;;
  esac
done
[[ -n $slug && -n $file && -f $file ]] || { echo "need <project-slug> <file.jsonl>" >&2; usage; }

marbles_bin="${MARBLES_BIN:-marbles}"
command -v "$marbles_bin" >/dev/null || { echo "$marbles_bin not on PATH" >&2; exit 3; }

# Derive the prefix from the file's own ids unless the caller passed one.
has_prefix=false
for ((i = 0; i < ${#extra[@]}; i++)); do
  [[ ${extra[i]} == --prefix* ]] && has_prefix=true
done
if ! $has_prefix; then
  derived=$(python3 -c '
import json, sys, collections
prefixes = collections.Counter()
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    row = json.loads(line)
    if row.get("_type") != "issue":
        continue
    rid = row.get("id", "")
    if "-" in rid:
        prefixes[rid.rsplit("-", 1)[0]] += 1
print(prefixes.most_common(1)[0][0] if prefixes else "")
' "$file")
  [[ -n $derived ]] && extra+=("--prefix" "$derived")
fi

echo "project=$slug file=$file flags=${extra[*]:-none}"
"$marbles_bin" import-bd "$file" --project "$slug" "${extra[@]+"${extra[@]}"}"

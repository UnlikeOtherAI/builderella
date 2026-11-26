#!/usr/bin/env bash
# List every acceptance id (A01..A68) that the test sources name, with the
# test that names it, so docs/acceptance.md can be checked against the code.
#
#   scripts/dev/acceptance.sh            one line per id and test
#   scripts/dev/acceptance.sh --ids      only the ids that have a test
#   scripts/dev/acceptance.sh --missing  only the ids that have none
#   scripts/dev/acceptance.sh --check    compare docs/acceptance.md with the
#                                        code; exit 1 and print what differs
#
# A test counts for an id when its function name carries it
# (`a27_deleted_file_...`) or the doc comment directly above it does
# (`/// A27: ...`). An id in a file's `//!` header is listed as
# `(module doc)`. Output columns, tab-separated: id, crate, test, file.
#
# Only Rust tests (`#[test]` and `#[tokio::test]`) under crates/ are read.
set -euo pipefail

cd "$(dirname "$0")/../.."
mode="${1:-}"
case "$mode" in
  "" | --ids | --missing | --check) ;;
  *)
    echo "usage: $0 [--ids|--missing|--check]" >&2
    exit 2
    ;;
esac

listing="$(
  find crates -name '*.rs' -not -path '*/target/*' -print0 |
    sort -z |
    xargs -0 awk '
      # Append to `found` every id in `text`. With `in_name` an id must be
      # a whole `_`-separated part of a function name; otherwise it must
      # not touch a letter, a digit or `_`.
      function ids(text, in_name,    rest, at, id, before, after, base, whole) {
        rest = text
        base = 0
        while ((at = match(rest, /[Aa][0-9][0-9]/)) > 0) {
          id = toupper(substr(rest, at, 3))
          before = (base + at == 1) ? "" : substr(text, base + at - 1, 1)
          after = substr(text, base + at + 3, 1)
          if (in_name) {
            whole = (before == "" || before == "_") && (after == "" || after == "_")
          } else {
            whole = before !~ /[A-Za-z0-9_]/ && after !~ /[A-Za-z0-9_]/
          }
          if (whole && id >= "A01" && id <= "A68") found[id] = 1
          base += at
          rest = substr(rest, at + 1)
        }
      }
      function emit(test,    id, crate) {
        crate = FILENAME
        sub(/^crates\//, "", crate)
        sub(/\/.*/, "", crate)
        for (id in found) printf "%s\t%s\t%s\t%s\n", id, crate, test, FILENAME
        split("", found)
      }
      FNR == 1 { split("", found); is_test = 0 }
      /^[ \t]*\/\/!/ { ids($0, 0); emit("(module doc)"); next }
      /^[ \t]*\/\/\// { ids($0, 0); next }
      /^[ \t]*#\[/ {
        if ($0 ~ /#\[(tokio::)?test/) is_test = 1
        next
      }
      /^[ \t]*(pub(\([a-z]+\))? +)?(async +)?fn +[A-Za-z0-9_]+/ {
        name = $0
        sub(/^.*fn +/, "", name)
        sub(/[^A-Za-z0-9_].*$/, "", name)
        if (is_test) { ids(name, 1); emit(name) }
        split("", found)
        is_test = 0
        next
      }
      /^[ \t]*$/ { next }
      { split("", found); is_test = 0 }
    ' |
    sort -u
)"

all_ids="$(awk 'BEGIN { for (n = 1; n <= 68; n++) printf "A%02d\n", n }')"
have="$(printf '%s\n' "$listing" | cut -f1 | sort -u)"

# The id and test of every row of the table in docs/acceptance.md: each
# backticked name in the last column that is not a crate name.
documented() {
  awk -F'|' '
    $2 ~ /^ *A[0-9][0-9] *$/ {
      id = $2
      gsub(/ /, "", id)
      rest = $(NF - 1)
      while (match(rest, /`[A-Za-z0-9_-]+`/)) {
        name = substr(rest, RSTART + 1, RLENGTH - 2)
        if (name !~ /^bella-/) printf "%s\t%s\n", id, name
        rest = substr(rest, RSTART + RLENGTH)
      }
    }
  ' docs/acceptance.md | sort -u
}

case "$mode" in
  --ids) printf '%s\n' "$have" ;;
  --missing) comm -23 <(printf '%s\n' "$all_ids") <(printf '%s\n' "$have") ;;
  --check)
    in_code="$(printf '%s\n' "$listing" | awk -F'\t' '$3 != "(module doc)" { print $1 "\t" $3 }' | sort -u)"
    in_doc="$(documented)"
    only_code="$(comm -23 <(printf '%s\n' "$in_code") <(printf '%s\n' "$in_doc"))"
    only_doc="$(comm -13 <(printf '%s\n' "$in_code") <(printf '%s\n' "$in_doc"))"
    rows="$(awk -F'|' '$2 ~ /^ *A[0-9][0-9] *$/ { gsub(/ /, "", $2); print $2 }' docs/acceptance.md | sort)"
    status=0
    if [ "$rows" != "$all_ids" ]; then
      echo "docs/acceptance.md does not have exactly one row for each of A01..A68" >&2
      status=1
    fi
    if [ -n "$only_code" ]; then
      echo "in the tests but not in docs/acceptance.md:" >&2
      printf '%s\n' "$only_code" >&2
      status=1
    fi
    if [ -n "$only_doc" ]; then
      echo "in docs/acceptance.md but not in the tests:" >&2
      printf '%s\n' "$only_doc" >&2
      status=1
    fi
    [ "$status" -eq 0 ] && echo "docs/acceptance.md matches the test sources"
    exit "$status"
    ;;
  *) printf '%s\n' "$listing" ;;
esac

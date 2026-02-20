#!/bin/bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"

if [ ! -f "$CARGO_TOML" ]; then
  echo "Cargo.toml not found at $CARGO_TOML" >&2
  exit 1
fi

bump_input="${1:-patch}"

current_version="$(python3 - "$CARGO_TOML" <<'PY'
import re
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as file:
    lines = file.readlines()

in_package = False
for line in lines:
    stripped = line.strip()
    if stripped == "[package]":
        in_package = True
        continue
    if in_package and stripped.startswith("[") and stripped != "[package]":
        break
    if in_package:
        match = re.match(r'^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$', stripped)
        if match:
            print(match.group(1))
            sys.exit(0)

raise SystemExit("Could not find [package] version in Cargo.toml")
PY
)"

IFS='.' read -r major minor patch <<<"$current_version"

case "$bump_input" in
  major)
    next_version="$((major + 1)).0.0"
    ;;
  minor)
    next_version="$major.$((minor + 1)).0"
    ;;
  patch)
    next_version="$major.$minor.$((patch + 1))"
    ;;
  *)
    if [[ "$bump_input" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      next_version="$bump_input"
    else
      echo "Invalid version bump '$bump_input'" >&2
      echo "Usage: ./scripts/release-tag.sh [major|minor|patch|X.Y.Z]" >&2
      exit 1
    fi
    ;;
esac

if [ "$current_version" = "$next_version" ]; then
  echo "Current version already $current_version" >&2
  exit 1
fi

tag_name="v$next_version"

if git rev-parse -q --verify "refs/tags/$tag_name" >/dev/null; then
  echo "Tag $tag_name already exists" >&2
  exit 1
fi

python3 - "$CARGO_TOML" "$current_version" "$next_version" <<'PY'
import re
import sys

path, current_version, next_version = sys.argv[1], sys.argv[2], sys.argv[3]
with open(path, "r", encoding="utf-8") as file:
    lines = file.readlines()

in_package = False
updated = False

for index, line in enumerate(lines):
    stripped = line.strip()
    if stripped == "[package]":
        in_package = True
        continue

    if in_package and stripped.startswith("[") and stripped != "[package]":
        break

    if in_package and re.match(r'^version\s*=\s*"[0-9]+\.[0-9]+\.[0-9]+"\s*$', stripped):
        lines[index] = re.sub(
            r'^version\s*=\s*"[0-9]+\.[0-9]+\.[0-9]+"\s*$',
            f'version = "{next_version}"',
            stripped,
        ) + "\n"
        updated = True
        break

if not updated:
    raise SystemExit("Failed to update [package] version in Cargo.toml")

with open(path, "w", encoding="utf-8") as file:
    file.writelines(lines)
PY

git tag "$tag_name"

echo "Bumped Cargo.toml version: $current_version -> $next_version"
echo "Created tag: $tag_name"
echo "Next: git add Cargo.toml && git commit -m \"release: $tag_name\" && git push && git push origin $tag_name"

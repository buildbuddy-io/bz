#!/usr/bin/env bash
set -euo pipefail

latest=$(git tag --list 'v*' --sort=-v:refname | head -1)

if [[ -z "$latest" ]]; then
  default="v0.1.0"
else
  # Bump patch version
  base="${latest#v}"
  IFS='.' read -r major minor patch <<< "$base"
  default="v${major}.${minor}.$((patch + 1))"
fi

read -rp "Release version [$default]: " version
version="${version:-$default}"

# Validate format
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Error: version must match vX.Y.Z" >&2
  exit 1
fi

if git rev-parse "$version" &>/dev/null; then
  echo "Error: tag $version already exists" >&2
  exit 1
fi

echo "Tagging $version and pushing..."
git tag "$version"
git push origin "$version"
echo "Release $version pushed. GitHub Actions will build and publish the release."

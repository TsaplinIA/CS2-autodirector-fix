#!/usr/bin/env bash
set -euo pipefail

repository=${1:?usage: merge-compatibility-pr.sh <repository> <branch>}
branch=${2:?usage: merge-compatibility-pr.sh <repository> <branch>}

gh pr merge --repo "$repository" --merge --delete-branch "$branch"

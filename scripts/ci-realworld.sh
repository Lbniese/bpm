#!/usr/bin/env bash
# Real-world corpus validation.
#
# Installs pinned upstream repositories that commit npm lockfiles, end to
# end, with `bpm import` + `bpm ci` against an isolated cold store, and
# asserts the exact installed package count for each. The count assertion is
# the regression gate for silent package-dropping (the inBundle import bug
# dropped 286 packages from npm/cli while exiting 0). Counts are true
# placements: platform-skipped and pruned-unreachable optionals are excluded.
#
# Repo-specific smoke checks encode the failure classes this corpus has
# already caught:
#   - jquery:    a declared bin executes from the project view
#   - cli:       workspace links stay live symlinks (npm shape) and a
#                root-project bundleDependency resolves
#   - socket.io: a hosted Git dependency (uWebSockets.js, no registry
#                integrity) imports as a commit-keyed codeload tarball
#   - vscode:    install scripts run in full — native modules build through
#                the managed node-gyp and load through the project view
#
# Corpus pins move only deliberately: update the commit and the expected
# count together, after verifying. Counts are the Linux/x86_64 reference —
# the CI job's platform — because platform filtering makes them
# OS/arch-specific (a local macOS run legitimately reports different
# numbers). The CI job's failure output prints the actual count to pin.
set -euo pipefail

# Absolutize: the loop cd's into each cloned repo, so a relative binary
# path would stop resolving.
BPM=${BPM:-./target/release/bpm}
case $BPM in
  /*) ;;
  *) BPM=$PWD/$BPM ;;
esac
WORK=${WORK:-$(mktemp -d)}

# url|commit|expected-installed|extra-ci-flags
CORPUS=(
  "https://github.com/jquery/jquery.git|c2f27864e4ffc2d1409c289fcb9a42b6165dfeaf|752|"
  "https://github.com/axios/axios.git|aa54a5e1e0e50cd1a2561b854ae5490ff589d065|635|"
  "https://github.com/istanbuljs/nyc.git|908620475199fa7b9ea0ea8b21d6d8ad6921e3ae|942|"
  "https://github.com/mochajs/mocha.git|cb21a4eba550da665fb2c3fa1ce021777a191e23|653|"
  "https://github.com/npm/cli.git|52f30938bd372dac25667f38fb771f5fe825e6e6|1149|"
  "https://github.com/socketio/socket.io.git|1eaa582d3b453e3e6f522300ed05b10da0a0799b|1461|"
  "https://github.com/microsoft/vscode.git|53b7b197990e6d9354dba0e42cce9b34d7c56c5c|1595|"
)

failures=0

clone_pinned() {
  local url=$1 commit=$2 dest=$3
  git init --quiet "$dest"
  git -C "$dest" remote add origin "$url"
  # GitHub advertises arbitrary SHAs (uploadpack.allowAnySHA1InWant), so a
  # depth-1 fetch by commit pins the tree without any history.
  git -C "$dest" fetch --depth 1 --quiet origin "$commit"
  git -C "$dest" checkout --quiet FETCH_HEAD
}

assert_installed_count() {
  local repo=$1 log=$2 expected=$3
  local installed
  installed=$(sed -n 's/.*installed \([0-9][0-9]*\) package(s).*/\1/p' "$log" | tail -1)
  if [ -z "$installed" ]; then
    echo "FAIL $repo: no 'installed N package(s)' summary in output" >&2
    return 1
  fi
  if [ "$installed" != "$expected" ]; then
    echo "FAIL $repo: installed $installed packages, expected $expected" \
      "(silent package drop or graph change — update the pin only after verifying)" >&2
    return 1
  fi
  echo "  $repo: $installed packages"
}

for entry in "${CORPUS[@]}"; do
  IFS='|' read -r url commit expected flags <<<"$entry"
  name=$(basename "$url" .git)
  repo_dir="$WORK/$name"
  log="$WORK/$name.log"
  echo "==> $name @ ${commit:0:12}"
  clone_pinned "$url" "$commit" "$repo_dir"
  if ! (cd "$repo_dir" && "$BPM" import >/dev/null 2>"$WORK/$name.import.err"); then
    echo "FAIL $name: bpm import exited nonzero" >&2
    tail -5 "$WORK/$name.import.err" >&2
    failures=$((failures + 1))
    continue
  fi
  # An isolated store per corpus run keeps the gate cold and hermetic: the
  # same downloads, extraction, and graph work happen on every run.
  if ! (cd "$repo_dir" && "$BPM" ci --store "$WORK/store" $flags >"$log" 2>&1); then
    echo "FAIL $name: bpm ci exited nonzero" >&2
    grep '^error' "$log" | head -3 >&2
    failures=$((failures + 1))
    continue
  fi
  assert_installed_count "$name" "$log" "$expected" || failures=$((failures + 1))
done

# ── repo-specific smokes ────────────────────────────────────────────────

# A declared bin executes through the project view (jquery dev toolchain).
if "$WORK/jquery/node_modules/.bin/eslint" --version >/dev/null 2>&1; then
  echo "  jquery: eslint bin executes"
else
  echo "FAIL jquery: node_modules/.bin/eslint --version exited nonzero" >&2
  failures=$((failures + 1))
fi

# Workspace links stay live symlinks in npm's relative shape.
arborist_link=$(readlink "$WORK/cli/node_modules/@npmcli/arborist" || true)
if [ "$arborist_link" = "../../workspaces/arborist" ]; then
  echo "  cli: workspace link is live (${arborist_link})"
else
  echo "FAIL cli: node_modules/@npmcli/arborist -> '${arborist_link:-missing}' (expected ../../workspaces/arborist)" >&2
  failures=$((failures + 1))
fi

# A root-project bundleDependency resolves (the inBundle import bug dropped
# all 286 of them, breaking template-oss's postinstall).
if [ -f "$WORK/cli/node_modules/proc-log/package.json" ]; then
  echo "  cli: root bundleDependency proc-log present"
else
  echo "FAIL cli: node_modules/proc-log missing (bundleDependencies dropped by import?)" >&2
  failures=$((failures + 1))
fi

# A native module built by the managed node-gyp loads through the project
# view — proves the lifecycle script PATH, node-gyp provisioning, and the
# volume's derived-content flow end to end.
if node -e "require('$WORK/vscode/node_modules/@vscode/fs-copyfile')" 2>/dev/null; then
  echo "  vscode: native @vscode/fs-copyfile loads"
else
  echo "FAIL vscode: native require('@vscode/fs-copyfile') exited nonzero (node-gyp build missing?)" >&2
  failures=$((failures + 1))
fi

echo
if [ "$failures" -ne 0 ]; then
  echo "real-world corpus: ${failures} failure(s)" >&2
  exit 1
fi
echo "real-world corpus: all entries passed"

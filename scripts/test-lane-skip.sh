#!/usr/bin/env bash
# Validate bored's lane-skip wiring and what the e2e lane's key covers (card #531).
#
# The lane-skip plugin owns the key, its .dockerignore matcher and the markers,
# and tests them in its own repo. This tests bored's side, where a mistake is
# silent: a marker written for a run that never passed, or a key that misses a
# file e2e reads, makes later pipelines skip e2e on a change nobody tested.
#
# Usage: scripts/test-lane-skip.sh <covered-files>
#   <covered-files> is the `app` lane's covered paths, one per line, as the
#   pinned plugin prints them in `files` mode (CI: the lane-files step writes
#   .lane/files/app.txt). Locally:
#     docker run --rm -v "$PWD:/w" -w /w -e PLUGIN_MODE=files \
#       -e PLUGIN_CONFIG=lanes.yml -e PLUGIN_LANE=app <lane-skip image> > /tmp/app.txt
#
# Wiring (.woodpecker/build.yml, lanes.yml):
#   1. lanes.yml lives at the repo root and .woodpecker/ holds no lanes.yml
#      (Woodpecker would parse it as a workflow and break every pipeline);
#      the `app` lane is the Dockerfile's context and gates exactly `e2e`.
#   2. every lane-skip plugin step names `config: lanes.yml` and gets
#      github_token, and every use of the image shares one digest pin.
#   3. the only gated step is `e2e`, and its first command is the exact skip
#      guard; no step reads a .lane/*.skip file anywhere else.
#   4. e2e depends on a `mode: check` step whose steps include e2e.
#   5. exactly one `mode: mark` step records e2e; it depends on e2e and has no
#      `when: status` override, so it runs only after e2e succeeded.
#   6. publish depends, directly or not, on every other push step but the
#      mark — so it runs only after every test that ran, e2e included.
# Coverage (the covered-file list, against `git ls-files` — never the plugin's
# own matcher, so a matcher bug cannot hide here):
#   7. every tracked file outside a short list of docs and agent/editor
#      config is covered — so e2e re-runs on any change it could see;
#   8. the docs a docs-only push changes (root *.md, LICENSE) are not covered
#      — so such a push skips e2e.
# Negative cases mutate the parsed inputs to prove each rule bites.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ne 1 ] || [ ! -f "$1" ]; then
    echo "usage: $0 <covered-files>  (the app lane's paths from lane-skip's files mode)" >&2
    exit 2
fi

TRACKED=$(mktemp)
trap 'rm -f "$TRACKED"' EXIT
git -c safe.directory='*' ls-files > "$TRACKED"

python3 - "$1" "$TRACKED" <<'PY'
import copy
import fnmatch
import os
import re
import sys

import yaml

COVERED_PATH, TRACKED_PATH = sys.argv[1], sys.argv[2]

IMAGE = re.compile(r"^ghcr\.io/vcheesbrough/woodpecker-plugin-lane-skip:\d+\.\d+\.\d+@sha256:[0-9a-f]{64}$")
GUARD = re.compile(r"\[ ! -e \.lane/(\S+)\.skip \] \|\| \{ cat \.lane/\1\.skip; exit 0; \}")
READS_SKIP = re.compile(r"\.lane/\S+\.skip")
GITHUB_TOKEN = {"from_secret": "github_token"}

# The only tracked files a push may change without re-running e2e: what the
# root .dockerignore leaves out of the build context. Patterns are matched
# against whole paths; `*.md` is the root only, as .dockerignore reads it.
MAY_SKIP = ["*.md", "LICENSE", ".claude/*", ".cursor/*", ".idea/*", ".run/*", ".cargo/*"]
# What a docs-only push changes: these must never be in the key.
DOCS = ["README.md", "AGENTS.md", "CLAUDE.md", "LICENSE", "LICENSE-TIER.md"]


def may_skip(path):
    return any(fnmatch.fnmatchcase(path, p) and ("/" not in path or "/" in p) for p in MAY_SKIP)


def events(step):
    return {e for rule in step.get("when") or [] for e in (rule.get("event") or [])}


def is_lane_image(step):
    return "woodpecker-plugin-lane-skip" in str(step.get("image", ""))


def wiring(build, lanes, woodpecker_files):
    problems = []
    steps = build.get("steps") or {}
    plugins = {n: s for n, s in steps.items() if is_lane_image(s) and s.get("settings")}

    # 1. lane config location and shape.
    if any(os.path.basename(f) in ("lanes.yml", "lanes.yaml") for f in woodpecker_files):
        problems.append(".woodpecker/ holds a lanes.yml, which Woodpecker parses as a workflow")
    lane = (lanes.get("lanes") or {}).get("app") or {}
    if lane.get("context") != "Dockerfile" or lane.get("steps") != ["e2e"]:
        problems.append(f"lanes.yml app lane is {lane}, want context Dockerfile and steps [e2e]")

    # 2. settings and the one pinned image.
    for name, step in plugins.items():
        settings = step["settings"]
        if settings.get("config") != "lanes.yml":
            problems.append(f"{name}: config is not lanes.yml")
        if settings.get("github_token") != GITHUB_TOKEN:
            problems.append(f"{name}: github_token is not from_secret github_token")
    images = {str(s.get("image")) for s in steps.values() if is_lane_image(s)}
    if len(images) != 1 or not all(IMAGE.match(i) for i in images):
        problems.append(f"lane-skip steps must share one digest-pinned image, got {sorted(images)}")

    # 3. the guard.
    gated = {}
    for name, step in steps.items():
        commands = [str(c) for c in step.get("commands") or []]
        if not any(READS_SKIP.search(c) for c in commands):
            continue
        match = GUARD.fullmatch(commands[0].strip())
        if not match:
            problems.append(f"{name}: reads a .lane/*.skip file but its first command is not the exact skip guard")
            continue
        gated[name] = match.group(1)
    if gated != {"e2e": "e2e"}:
        problems.append(f"gated steps are {gated}, want {{'e2e': 'e2e'}}")

    for name, lane_step in gated.items():
        step = steps[name]
        # 4. check before the gated step.
        checks = [
            d for d in step.get("depends_on") or []
            if d in plugins
            and plugins[d]["settings"].get("mode") == "check"
            and lane_step in (plugins[d]["settings"].get("steps") or [])
            and events(plugins[d]) >= events(step)
        ]
        if not checks:
            problems.append(f"{name}: depends on no lane-check (on its events) whose steps include {lane_step}")
        # 5. mark only after a pass.
        marks = [n for n, s in plugins.items() if s["settings"].get("mode") == "mark" and s["settings"].get("step") == lane_step]
        if len(marks) != 1:
            problems.append(f"{name}: {len(marks)} mark steps record {lane_step}, want one")
            continue
        mark = plugins[marks[0]]
        if name not in (mark.get("depends_on") or []):
            problems.append(f"{marks[0]}: does not depend on {name}")
        if any("status" in rule for rule in mark.get("when") or []):
            problems.append(f"{marks[0]}: overrides `when: status`, so it could mark a failure")

    # 6. publish after every test that ran.
    publish = steps.get("publish") or {}
    marks = {n for n, s in plugins.items() if s["settings"].get("mode") == "mark"}

    def ancestors(name, seen=None):
        seen = set() if seen is None else seen
        for dep in steps.get(name, {}).get("depends_on") or []:
            if dep not in seen:
                seen.add(dep)
                ancestors(dep, seen)
        return seen

    before_publish = ancestors("publish")
    for name, step in steps.items():
        if name == "publish" or name in marks or not (events(step) & events(publish)):
            continue
        if name not in before_publish:
            problems.append(f"publish does not wait for {name}")
    return problems


def coverage(covered, tracked):
    problems = []
    for path in tracked:
        if path not in covered and not may_skip(path):
            problems.append(f"{path} is not in the app lane's key, so e2e would skip a change to it")
    for path in DOCS:
        if path in covered:
            problems.append(f"{path} is in the app lane's key, so a docs-only push would not skip e2e")
    for path in covered:
        if path not in tracked:
            problems.append(f"{path} is covered but not tracked")
    return problems


build = yaml.safe_load(open(".woodpecker/build.yml"))
lanes = yaml.safe_load(open("lanes.yml"))
woodpecker_files = sorted(os.listdir(".woodpecker"))
covered = {l.strip() for l in open(COVERED_PATH) if l.strip()}
tracked = {l.strip() for l in open(TRACKED_PATH) if l.strip()}
failed = False


def report(ok, message):
    global failed
    print(("ok   " if ok else "FAIL ") + message)
    failed |= not ok


problems = wiring(build, lanes, woodpecker_files) + coverage(covered, tracked)
for p in problems:
    print("     " + p)
report(not problems, "the pipeline's lane-skip wiring and the app lane's coverage hold")
report(len(covered) > 100, f"the covered list is a real listing ({len(covered)} files)")


def bites(label, mutate, rule):
    b, l, w = copy.deepcopy(build), copy.deepcopy(lanes), list(woodpecker_files)
    c, t = set(covered), set(tracked)
    mutate(b, l, w, c, t)
    found = wiring(b, l, w) + coverage(c, t)
    report(any(rule in p for p in found), f"rejects {label}")


s = lambda b: b["steps"]
bites("a lanes.yml inside .woodpecker/", lambda b, l, w, c, t: w.append("lanes.yml"), "Woodpecker parses")
bites("a lane without the Dockerfile context", lambda b, l, w, c, t: l["lanes"]["app"].pop("context"), "app lane is")
bites("a plugin step with no config", lambda b, l, w, c, t: s(b)["lane-mark-e2e"]["settings"].pop("config"), "config is not lanes.yml")
bites("a plugin step with no token", lambda b, l, w, c, t: s(b)["lane-check"]["settings"].pop("github_token"), "github_token")
bites("an unpinned plugin image", lambda b, l, w, c, t: s(b)["lane-files"].update(image="ghcr.io/vcheesbrough/woodpecker-plugin-lane-skip:0.1.0"), "digest-pinned")
bites("the guard not first", lambda b, l, w, c, t: s(b)["e2e"]["commands"].reverse(), "not the exact skip guard")
bites("no guard on e2e", lambda b, l, w, c, t: s(b)["e2e"]["commands"].pop(0), "gated steps are")
bites("e2e not waiting for lane-check", lambda b, l, w, c, t: s(b)["e2e"]["depends_on"].remove("lane-check"), "depends on no lane-check")
bites("lane-check not deciding e2e", lambda b, l, w, c, t: s(b)["lane-check"]["settings"].update(steps=["other"]), "depends on no lane-check")
bites("a second mark", lambda b, l, w, c, t: s(b).update({"mark-2": copy.deepcopy(s(b)["lane-mark-e2e"])}), "mark steps record")
bites("a mark not after e2e", lambda b, l, w, c, t: s(b)["lane-mark-e2e"].update(depends_on=["build"]), "does not depend on e2e")
bites("a mark that runs on failure", lambda b, l, w, c, t: s(b)["lane-mark-e2e"]["when"].append({"status": ["success", "failure"]}), "when: status")
bites("publish not waiting for e2e", lambda b, l, w, c, t: s(b)["publish"]["depends_on"].remove("e2e"), "publish does not wait for e2e")
bites("publish not waiting for lane-validation", lambda b, l, w, c, t: s(b)["publish"]["depends_on"].remove("lane-validation"), "publish does not wait for lane-validation")
bites("an e2e input missing from the key", lambda b, l, w, c, t: c.discard("e2e/docker-compose.test.yml"), "e2e/docker-compose.test.yml is not in")
bites("the pipeline missing from the key", lambda b, l, w, c, t: c.discard(".woodpecker/build.yml"), ".woodpecker/build.yml is not in")
bites("backend source missing from the key", lambda b, l, w, c, t: c.discard("backend/Cargo.toml"), "backend/Cargo.toml is not in")
bites("README.md in the key", lambda b, l, w, c, t: c.add("README.md"), "README.md is in")
bites("a nested Markdown file left out", lambda b, l, w, c, t: t.add("e2e/NOTES.md"), "e2e/NOTES.md is not in")

sys.exit(1 if failed else 0)
PY

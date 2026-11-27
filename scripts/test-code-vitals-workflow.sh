#!/usr/bin/env bash
# Validate .woodpecker/code-vitals.yml without running it (card #567).
#
# The workflow records bored's trunk commits for code-vitals; its one promise
# to bored is that it can never fail or hold up a build. This holds the shape
# that promise rests on:
#   1. it runs on `main` push / manual only (never a PR, never a deployment);
#   2. it is one step, `failure: ignore`, with no `depends_on` of its own;
#   3. no other workflow depends on it;
#   4. the step is the code-vitals plugin (code-vitals #564), pulled on every
#      run (its tag moves), and runs no commands of its own;
#   5. its settings are ones the plugin knows, its trunk is the branch the
#      workflow runs on, and the writer password comes by name from the broker,
#      never inline, as the only secret the step receives.
# Rule 5 catches only a misspelt setting name and a wrong trunk. The values of
# version_sources and areas, and an area-rule change without a new
# areas.version, are validated by code-vitals (src/code_vitals/settings.py, and
# its deploy for the version), which here would only fail an ignored step.
# Negative cases mutate the parsed workflows to prove each rule bites.
#
# Usage: scripts/test-code-vitals-workflow.sh   (needs python3 + PyYAML)

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import copy
import glob
import re
import sys

import yaml

CODE_VITALS = ".woodpecker/code-vitals.yml"
NAME = "code-vitals"
TRUNK = "main"
PLUGIN = re.compile(r"^registry\.desync\.link/code-vitals-plugin(:[\w.-]+)?(@sha256:[0-9a-f]{64})?$")
# code-vitals' src/code_vitals/settings.py `_KEYS`, which refuses any other key,
# less `clone_token`: that is for private repos only, and bored is public, so
# a token here would only hand the step a second secret (rule 5).
SETTINGS = {"trunk", "version_sources", "areas", "writer_password"}
WRITER = {"from_secret": "code_vitals_prod_writer_password"}


def secrets(node):
    if isinstance(node, dict):
        found = {node["from_secret"]} if "from_secret" in node else set()
        for value in node.values():
            found |= secrets(value)
        return found
    if isinstance(node, list):
        return set().union(*(secrets(v) for v in node)) if node else set()
    return set()


def as_list(value):
    if value is None:
        return []
    return [value] if isinstance(value, str) else list(value)


def check(workflows):
    problems = []
    workflow = workflows.get(CODE_VITALS)
    if workflow is None:
        return [f"{CODE_VITALS} is missing"]
    when = workflow.get("when")
    # No rules (absent or an empty list) means no limit: every event runs it.
    rules = when if isinstance(when, list) else [when or {}]
    if not rules:
        problems.append(f"{CODE_VITALS}: an empty `when` runs on every event")
    for rule in rules:
        if set(as_list(rule.get("event"))) != {"push", "manual"}:
            problems.append(f"{CODE_VITALS}: runs on {rule.get('event')!r}, not push/manual only")
        if rule.get("branch") != TRUNK:
            problems.append(f"{CODE_VITALS}: not limited to {TRUNK} (branch {rule.get('branch')!r})")
    if workflow.get("depends_on"):
        problems.append(f"{CODE_VITALS}: waits on other workflows")
    steps = workflow.get("steps") or {}
    if len(steps) != 1:
        problems.append(f"{CODE_VITALS}: has {len(steps)} steps, not one")
    for name, step in steps.items():
        if step.get("failure") != "ignore":
            problems.append(f"{CODE_VITALS}: step {name} can fail the pipeline")
        if step.get("depends_on"):
            problems.append(f"{CODE_VITALS}: step {name} has depends_on")
        if not PLUGIN.match(str(step.get("image", ""))):
            problems.append(f"{CODE_VITALS}: step {name} is not the code-vitals plugin ({step.get('image')!r})")
        if step.get("pull") is not True:
            problems.append(f"{CODE_VITALS}: step {name} may run a stale copy of the moving plugin tag (no pull: true)")
        if step.get("commands") or step.get("entrypoint"):
            problems.append(f"{CODE_VITALS}: step {name} runs commands of its own")
        settings = step.get("settings") or {}
        unknown = sorted(set(settings) - SETTINGS)
        if unknown:
            problems.append(f"{CODE_VITALS}: step {name} has setting(s) the plugin refuses or bored must not send: {unknown}")
        branches = {rule.get("branch") for rule in rules}
        if {settings.get("trunk")} != branches:
            problems.append(f"{CODE_VITALS}: trunk {settings.get('trunk')!r} is not the branch it runs on")
        if settings.get("writer_password") != WRITER:
            problems.append(f"{CODE_VITALS}: step {name} does not take the writer password from the broker")
        held = secrets(step)
        if held != {WRITER["from_secret"]}:
            problems.append(f"{CODE_VITALS}: step {name} receives secrets {sorted(held)}, not the writer password only")
    for path, other in workflows.items():
        if path != CODE_VITALS and NAME in as_list((other or {}).get("depends_on")):
            problems.append(f"{path}: depends on {NAME}")
    return problems


workflows = {p: yaml.safe_load(open(p)) for p in glob.glob(".woodpecker/*.yml")}
failed = False


def report(ok, message):
    global failed
    print(("ok   " if ok else "FAIL ") + message)
    failed |= not ok


problems = check(workflows)
for p in problems:
    report(False, p)
report(not problems, f"code-vitals runs on {TRUNK} pushes only and can never block a build")


def step(w):
    (only,) = w[CODE_VITALS]["steps"].values()
    return only


def mutated(change):
    copied = copy.deepcopy(workflows)
    change(copied)
    return check(copied)


def on_pull_requests(w):
    w[CODE_VITALS]["when"] = [{"event": ["push", "manual", "pull_request"], "branch": TRUNK}]


def on_deployments(w):
    w[CODE_VITALS]["when"] = [{"event": ["push", "manual", "deployment"], "branch": TRUNK}]


def any_branch(w):
    w[CODE_VITALS]["when"] = [{"event": ["push", "manual"]}]


def empty_when(w):
    w[CODE_VITALS]["when"] = []


def can_fail(w):
    step(w).pop("failure")


def second_step(w):
    w[CODE_VITALS]["steps"]["more"] = dict(step(w))


def waits_on_others(w):
    w[CODE_VITALS]["depends_on"] = ["build"]


def step_waits(w):
    step(w)["depends_on"] = ["setup"]


def awaited(w):
    w[".woodpecker/build.yml"]["depends_on"] = [NAME]


def inline_password(w):
    step(w)["settings"]["writer_password"] = "hunter2"


def not_the_plugin(w):
    step(w)["image"] = "registry.desync.link/code-vitals-collector:stable"


def no_pull(w):
    step(w).pop("pull")


def own_commands(w):
    step(w)["commands"] = ["echo hi"]


def misspelt_setting(w):
    step(w)["settings"]["version_source"] = step(w)["settings"].pop("version_sources")


def clone_token(w):
    step(w)["settings"]["clone_token"] = {"from_secret": "github_token"}


def wrong_trunk(w):
    step(w)["settings"]["trunk"] = "master"


def extra_secret(w):
    step(w)["environment"] = {"REGISTRY_PASSWORD": {"from_secret": "zot_ci_password"}}


def missing(w):
    w.pop(CODE_VITALS)


for label, change in [
    ("a pull request trigger", on_pull_requests),
    ("a deployment trigger", on_deployments),
    (f"a workflow not limited to {TRUNK}", any_branch),
    ("an empty when, which runs on every event", empty_when),
    ("a workflow that waits on others", waits_on_others),
    ("a step with depends_on", step_waits),
    ("a step that can fail the pipeline", can_fail),
    ("a second step", second_step),
    ("another workflow waiting on it", awaited),
    ("an inline password", inline_password),
    ("a step that is not the plugin", not_the_plugin),
    ("a plugin step running its own commands", own_commands),
    ("a moving tag without pull: true", no_pull),
    ("a setting the plugin would refuse", misspelt_setting),
    ("a clone token on this public repo", clone_token),
    ("a trunk other than the branch it runs on", wrong_trunk),
    ("a secret beyond the writer password", extra_secret),
    ("the workflow file gone", missing),
]:
    report(bool(mutated(change)), f"refuses {label}")

sys.exit(1 if failed else 0)
PY

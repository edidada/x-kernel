---
name: pr-ci-review
description: Use when creating or updating a Gitee Pull Request for X-Kernel, checking the latest CI status of a PR (Jenkins x-kernel-ci-test), checking a PR's xreview AI review, or when asked to request an AI review or respond to an AI finding. AI review state comes from xreview's status API; human opinions and CI reports from the Gitee PR comment area.
---

# PR CI and Review Tracking

Use this skill when an AI agent needs to:

- create or update a Pull Request against `openkylin/x-kernel`;
- find the latest Jenkins CI build for a given PR number;
- check whether the CI build is still running, what stage it is on, or why it failed;
- check a PR's AI review (xreview), request an AI review, or respond to an
  AI finding;
- read human review opinions and CI reports from the Gitee PR comment area.

## Key Context

- Code repository: `https://gitee.com/openkylin/x-kernel` (Gitee, PR-based workflow)
- CI job: Jenkins `x-kernel-ci-test` at `https://jenkins.openkylin.top/job/x-kernel-ci-test/`
- AI review: xreview at `https://review.x-kernel.com:57482` (the port is part
  of the URL; the bare host returns 403), triggered by the Gitee webhook on
  every push. Each PR has a review page at
  `https://review.x-kernel.com:57482/r/prs/<n>` and agent instructions at
  `https://review.x-kernel.com:57482/r/prs/<n>/agent.md` (generic version:
  `https://review.x-kernel.com:57482/r/agent.md`). agent.md is the source of
  truth for the review API; read it instead of copying calls from here.
- xreview posts one comment per PR as user `x-cibot` and edits it in place;
  its HTML markers start with `<!-- xreview:` (`summary`, `status`, `hint`).
  There are no inline comments.
- Older `x-cibot` comments marked `<!-- x-review:summary -->` /
  `<!-- x-review:review -->` are history from the retired `ai-review` job.
- CI status is also posted back to the PR as comments by user `openkylin-cibot`
  with an HTML comment marker `<!-- x-kernel-ci -->`.

## Core Rules

### Rule 1: AI review state comes from xreview's status call

The verdict, the blocking items and their ids come from xreview's status call
(section D). The `x-cibot` comment is only a summary that points to the review
page and agent.md. Human opinions and CI reports come from the Gitee PR
comment area (`pulls/<n>/comments`).

### Rule 2: Anonymous access is enough for status, opinions and xreview calls

Both the Jenkins API and the Gitee API v5 are readable without credentials
for this project (state-changing actions need a Jenkins account, see below).
All xreview calls, POSTs included, are unauthenticated, but every POST must
carry the header `X-Xreview-Client: <name>[/<version>]`.

### Rule 3: Control actions require Jenkins auth; never do them passively

Stop / terminate / re-trigger / replay endpoints all return `403` (CSRF gate)
or `401` (auth) anonymously. They need a signed-in Jenkins user with Build
permission plus a CSRF crumb. Do not probe control endpoints speculatively with
real credentials; only act when the user explicitly asks.

### Rule 4: Jenkins keeps only the most recent 31 builds

The `x-kernel-ci-test` job discards old builds. The API returns at most the
latest 31 builds (both `builds` and `allBuilds`). Older builds' console logs,
artifact paths, and `wfapi` stage data are gone (`404`), so failure details can
only be diagnosed for builds still inside the 31-build window.

### Rule 5: A PR number maps to builds via the Gitee webhook cause

Every build's `CauseAction` records `Triggered by (Debin) Gitee Pull Request
#<n>: <branch> => <base>`. Filter on that string to map PR number to build
numbers. The list is ordered newest first, so take the first match for "latest".

## Standard Workflow

### A. Create or update a PR

1. Fetch latest `main` and create a topic branch:

   ```bash
   git fetch origin
   git checkout -b fix/your-topic origin/main
   ```

2. Make the change, then push to Gitee:

   ```bash
   git add -A && git commit -m "fix(sub): meaningful summary"
   git push -u origin fix/your-topic
   ```

3. Open the PR with the `ge` CLI (Gitee v5 API wrapped; already authenticated
   in this environment as user `luodeb`):

   ```bash
   # from inside the repo (repo is auto-detected from git remote),
   # head branch = the branch you pushed, base = main
   ge pr create --title "fix(sub): meaningful summary" \
     --body "Describe what changed, why, and the expected behavior;
reference related issues with 'Fixes #IJSXXX' if any." \
     --head fix/your-topic --base main

   # body from a file is better for longer descriptions:
   ge pr create --title "fix(sub): meaningful summary" \
     --body-file ./pr-body.md --head fix/your-topic --base main

   # auto-fill title/body from the HEAD commit (subject + body):
   ge pr create --fill --head fix/your-topic --base main
   ```

   Equivalent raw Gitee v5 API call (same data the CLI sends):

   ```bash
   curl -sk -X POST "https://gitee.com/api/v5/repos/openkylin/x-kernel/pulls" \
     -d "access_token=$GITEE_TOKEN" \
     -d "title=fix(sub): meaningful summary" \
     -d "head=fix/your-topic" \
     -d "base=main" \
     -d "body=..."
   ```

   Verify creation with `ge pr view --repo openkylin/x-kernel` or the
   pull's `number` in the API response.

4. After the PR is open, the Gitee webhook automatically triggers:
   - Jenkins `x-kernel-ci-test` (build pipeline);
   - an xreview round on the head commit (posts or updates the xreview comment).
     Large PRs above the approval threshold wait for a maintainer; the author
     (or the author's agent) can start a round from the review page or with
     `POST /api/v1/prs/<n>/review` (section D).

5. Update the PR (new commits / `--amend`) pushes again; CI re-runs and
   xreview reviews the new head (a follow-up round against the previous one);
   then work through section D.

### B. Get the latest CI status for a PR

```bash
PR=759
BASE="https://jenkins.openkylin.top/job/x-kernel-ci-test"

# 1. List builds of this PR (newest first); take the first one:
curl -sk "$BASE/api/json?tree=builds%5Bnumber,result,url,actions%5Bcauses%5BshortDescription%5D%5D%5D" |
  python3 -c "
import json,sys
pr='$PR'
hits=[b for b in json.load(sys.stdin)['builds']
      for a in b.get('actions',[]) if a.get('_class')=='hudson.model.CauseAction'
      for c in a.get('causes',[]) if '#%s'%pr in c.get('shortDescription','')]
for h in hits: print(h['number'], h.get('result') or 'RUNNING', h['url'])
"

# 2. Build overview of the latest build (result == null while running):
#    GET $BASE/<n>/api/json?tree=number,building,result,duration,estimatedDuration,url

# 3. Stage-level progress (running stage shows IN_PROGRESS):
#    GET $BASE/<n>/wfapi/describe

# 4. Full console log:
#    GET $BASE/<n>/consoleText
```

### C. Diagnose why a CI build failed

1. Read the stage statuses from `wfapi/describe`; find the stage with
   `FAILED` (later stages may be `ABORTED` as a cascade).
2. Grep the console (`consoleText`) for the failing case:
   `grep -B3 -A3 "FAILED\|timed out"`.
3. Use the published artifacts for per-case precision
   (`artifact/.ci/` is open to anonymous GET):

   ```bash
   BASE="https://jenkins.openkylin.top/job/x-kernel-ci-test"
   BU=945
   # per-stage logs:
   #   $BASE/$BU/artifact/.ci/stage-logs/<Stage_Name>.log
   # per-test-case logs (get path from the stage log or artifacts tree):
   #   $BASE/$BU/artifact/.ci/work/runtime-<arch>/test-harness/logs/ci/<ts>/cases/<case>.log
   # host-companion logs:
   #   $BASE/$BU/artifact/.ci/work/runtime-<arch>/test-harness/logs/ci/<ts>/artifacts/<case>/host-ca.log
   # enumerate artifacts:
   #   $BASE/$BU/api/json?tree=artifacts[relativePath]
   ```

   Artifact tree is large (tens of thousands of entries); prefer URL-guessing
   from the stage log's `Log:` lines instead of loading the whole tree.

4. Determine flake vs regression by comparing the same stage/case in the
   previous successful builds still inside the 31-build window.

### D. Handle the AI review and read review opinions

Follow `https://review.x-kernel.com:57482/r/prs/<n>/agent.md`. In short:

1. GET the status: state, the latest round's verdict, and every item with its
   `item_id`.
2. For each blocking item, either fix it and push, or POST a response:
   `false_positive` with `file:line` evidence, `wont_fix` (still blocks the
   merge), or `recheck` for items of kind `inconclusive`.
3. After pushing fixes, POST a review request for the new head.
4. Re-read the status and check each response's `state`.

```bash
PR=759
XR="https://review.x-kernel.com:57482/api/v1/prs/$PR"
H='X-Xreview-Client: <agent>/<version>'
J='Content-Type: application/json'
curl -s "$XR/status"
curl -s -X POST "$XR/items/<item>/response" -H "$H" -H "$J" \
  -d '{"kind": "false_positive", "reason": "why, with file:line evidence"}'
curl -s -X POST "$XR/review" -H "$H" -H "$J" -d '{"note": "pushed fixes"}'
curl -s -X POST "$XR/recheck" -H "$H" -H "$J" -d '{}'  # all inconclusive items
```

For human opinions and CI reports, list the PR comments:

```bash
PR=759
# Anonymous, no token needed. Newest page ordered by id desc.
curl -sk "https://gitee.com/api/v5/repos/openkylin/x-kernel/pulls/$PR/comments?per_page=100&sort=id&order=desc" |
  python3 -c "
import json,sys
for c in json.load(sys.stdin):
    print('-', c['id'], c['user']['login'], (c.get('created_at') or '')[:19])
    print('  ', (c.get('body') or '').replace(chr(10),' ')[:140])
"
```

Classify each comment by user and marker prefix:

- `x-cibot` + `<!-- xreview:`: the xreview summary (verdict 可合并 / 需修改 /
  需复核, stale marker when the PR has newer commits); a pointer only.
- `x-cibot` + `<!-- x-review:`: the retired Jenkins `ai-review` job; history only.
- `openkylin-cibot` + `<!-- x-kernel-ci -->`: CI result report posted to PR.
- any other user (e.g. a `rerun` command by a maintainer): human opinion/command.

The status call is authoritative for the AI review; human comments can
override it, so check them before reporting a verdict.

## Prerequisites

- For creating/updating PRs: a Gitee account with SSH key or push credentials.
- For Jenkins control actions (stop/rerun): a Jenkins user with Build
  permission. Get the CSRF crumb first, then POST:

  ```bash
  CRUMB=$(curl -sk -u "$USER:$TOKEN" "https://jenkins.openkylin.top/crumbIssuer/api/json" \
    | python3 -c "import json,sys;print(json.load(sys.stdin)['crumb'])")
  # stop build <n>:  POST /job/x-kernel-ci-test/<n>/stop
  # terminate       POST /job/x-kernel-ci-test/<n>/terminate
  # re-trigger with same parameters:
  #   POST /job/x-kernel-ci-test/buildWithParameters
  #   (params: PROJECT_REPO, DEFAULT_BRANCH, LIBUTEE_REPO, TEST_HARNESS_REPO,
  #    TEST_HARNESS_BRANCH, ENABLE_DOC_CHECK)
  ```

  Detailed endpoint reference: [references/jenkins-gitee-api.md](references/jenkins-gitee-api.md).

## Validation Expectations

At minimum, report:

- the PR number and its head commit SHA;
- the latest build number/result and which stage failed (or is running);
- for failures: the failing stage, failing case, and the key error line from
  the per-case or host-companion log;
- the latest review opinion: xreview's verdict for the current head (from the
  status call) and any human comments, citing their Gitee comment ids;
- the xreview item ids handled and the response or recheck sent for each.
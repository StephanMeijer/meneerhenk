# Meneer Henk: specification

**Status:** describes behaviour as of 2026-10-02 (the `main` branches of the
Henk repositories). Features still on a branch are marked *(in development)*.
This document says **what Henk does and promises**, not how it is built.

---

## 1. What Henk is

Meneer Henk is the team's AI colleague: a strict, dry, senior software engineer
and code reviewer with a fixed persona. He works where the team works:

| Where | What he does there |
|---|---|
| GitHub pull requests | Reviews every new commit and pushes the fixes of what holds to its branch (§3.2, §3.5); answers when mentioned; reviews again on request |
| GitLab merge requests (9xxlab) | The same, GitLab's way |
| GitHub / GitLab issues | Plans how to carry out an issue, and triages it, when a colleague asks in Discord (§4); takes an issue to a pull request when asked (§4a) |
| Discord (his own channel) | Talks with the team; looks things up; starts reviews and plans; manages issues |
| Email (his own mailbox) | Answers the mail he receives |
| Discord voice channel *(in development)* | Listens, and answers out loud |

He is **advisory**. He never approves, blocks or merges. He changes code
only as the executor: in a review loop (§3.2), pushing the fixes of his
review to that pull request's own branch (§3.5), and when asked to take an
issue to a pull request (§4a), pushing a new branch of his own and opening
a pull request for it. Never to a fork, the default branch or a protected
branch.

### 1.1 Terms

| Term | Meaning |
|---|---|
| **Run** | One execution of a review, a plan, an execute run (§4a), a ticket run (§4b), a Discord turn or a mail reply. Every run has a link. Address runs from before #327 stay readable as runs. |
| **Ticket run** | A run that takes one issue through a plan, an execute run and a review of the pull request, and owns the three (§4b). |
| **Review loop** | What a review is: a reviewer and an executor taking turns on one pull request (§3.2). |
| **Reviewer** | The model session in a review loop that reviews the change and hands its findings to the executor (§3.2). |
| **Executor** | The model session that changes code: in a review loop it checks each finding against the code and fixes the ones that hold (§3.2); in an execute run it takes an issue to a pull request (§4a). |
| **Lane** | Before #309, one independent reviewer inside a review. Old runs still show them, as sessions with the history role `lane` (#324). |
| **Finding** | One problem the reviewer reports, at one line. Before #309 a lane posted each as a comment. |
| **Summary** | What the check says when a review ends (§3.3). |
| **Turn** | One unit of Discord work: one message (or a quick burst from one person) and Henk's answer to it (§5.2). |
| **Colleague** | A Discord user with the "Collega van Henk" role (§2). |
| **Team Lead** | The Discord user ids listed as such (§2). |
| **Requester** | The person whose request started a run. Their permissions bound the run (§8.3). |

---

## 2. People and permissions

| Who | How recognised | What they may have Henk do |
|---|---|---|
| **Anyone** who can comment on a pull/merge request Henk watches | Platform account (a person, not a bot) | Ask for a re-review; mention him |
| **Anyone** in Henk's Discord channel | Discord user | Talk to him; have him read repositories, issues and the web |
| **Colleague** ("Collega van Henk" Discord role) | Discord role id | Additionally: start reviews and plans, create, comment on and change issues, use Henk's mailbox |
| **Team Lead** (listed by Discord user id) | Discord user id, never a name | Additionally: *sudo* (§5.4). Their decisions are final to Henk |
| **Known people** (a roster by Discord user id) | Discord user id | Henk knows their name and role |
| **Anyone on the internet** | Email sender | Have their email answered |
| **MCP clients** (agents with a configured token) | A named bearer token, never a name | With a *read* token: read runs, events and health. With a *write* token: additionally start reviews and plans, and cancel runs. Recorded as `mcp:<token name>` |

Rules:

- Identity is always checked by a stable id (user id, role id, account id),
  never by a display name.
- A permission belongs to the person who asked, for that one request only.
  Nothing carries over to the next request.
- Henk works only in the repositories on an **allowlist**. Today that is
  `StephanMeijer/scratch-repo`, the `docspec` and `NotedThat` GitHub
  accounts, and the `9xxlab` GitLab group. Outside it he says he does not
  work there.

---

## 3. Code review

### 3.1 When he reviews

- A pull/merge request is **opened** or **reopened** (on GitLab: not a draft).
- **New commits** are pushed to an open one.
- Someone posts a comment whose whole text is `@meneer-henk review` (GitLab:
  `@meneerhenk review`) on an open one. He reacts 👀 to show he saw it, then
  reviews the latest commit.
- A colleague asks for it in Discord (`/henk review <target>` or in
  conversation), for one or more pull/merge requests (at most 50 at once).

A review always covers **one commit**: the head at the time. It reviews the
change against the target branch.

### 3.2 How he reviews

A review is one **review loop** (`[review.loop]`, #283): two model
sessions, a **reviewer** and an **executor**, working on one pull request in a
checkout of it. Reviews need `[review.loop]` with `[roles.reviewer]` and
`[roles.executor]` (#322); without `[review.loop]` Henk does not review
(#309). A review that cannot get its checkout does not
complete.

- **The reviewer reviews** the commit. It reads the change and as much of
  the surrounding code (callers, tests, configuration) as it needs, and may
  run commands in its checkout.
- It reports **only real problems the change introduces**: bugs, security
  issues, broken behaviour. No style remarks, no nitpicks.
- **Some files are not reviewed:** lockfiles and generated changelogs by
  default, or whatever is configured instead. They are listed as not
  reviewed and carry no finding. A change made only of such files is
  reported as "nothing to review", which is a completed review.
- **The reviewer hands its findings straight to the executor.** Nothing is
  posted on the pull request: no comment, no summary.
- **The executor checks each finding against the code** and gives it one
  verdict: *fixed*, *rejected* (it does not hold, and why) or *won't fix*
  (it holds but is not for this pull request, and why). A finding that has
  no verdict when the loop ends, or whose fix was never pushed, is
  *unsettled*.
- **Henk pushes the executor's changes** to the pull request's branch. The
  executor runs the repository's own checks; Henk trusts it, and the pull
  request's CI is the final word (§3.5).
- **The reviewer reviews again.** It resumes its own conversation, is told
  which commits were added, and reviews as it sees fit. New findings go back
  to the executor, which resumes its own conversation too.
- This repeats until the reviewer finds nothing more, or a limit ends it: a
  number of rounds, or a time limit per round and in all.
- What the loop did and the findings that still stand (won't fix and
  unsettled) are in the check (§3.3) and the run. Each finding and its
  verdict are in the run record.

A pull request the loop may not push to (a fork, the default or a protected
branch) or whose branch moved after the review was queued is reviewed by
the reviewer alone: one round at the reviewed commit, nothing pushed, and
its findings stand in the check with why they were not fixed (#307). On
GitLab, whose commit status carries only the count, that summary is
posted as the summary note as well.

Before #309 a review was a set of independent **lanes**, one model each,
that posted their findings as line comments after a second model
fact-checked them. Lanes and the fact-check are gone. Their comments stay on
the pull requests, and their markers are still recognised (§8.1).

### 3.3 What the pull/merge request shows

| Moment | GitHub | GitLab |
|---|---|---|
| Review waits for a slot | The check "Meneer Henk", *queued* (grey), "Waiting for a review slot", linked to the dashboard's overview, which lists it. A review that gets a slot at once skips this | Nothing: a pending commit status could block a merge where pipelines must succeed |
| Review starts | The check "Meneer Henk", *in progress*, linked to the run: the queued one moves on, or a new one | Henk's award emoji on the MR *(in development)* |
| During | Nothing but the commits Henk pushes for the executor (§3.2) | The same |
| Review ends | The check's **summary**: "No issues found" or "N issues found", and the loop's report: how many rounds it ran, what it settled and pushed, why it stopped, and the findings that still stand | No note, but for the reviewer alone (§3.2), which posts its summary as a note. The commit status below |
| Review ends | The check completes: success (no issues), neutral (issues found: advisory, never blocks a merge), or failure (the review did not complete). When the loop pushed, the last commit it pushed gets the same check. A review superseded by a newer commit completes neutral with "Superseded by a newer commit." A queued check never stays queued: cancelled while waiting, it completes neutral; superseded, neutral as above; Henk stopping, as an interrupted review; and a pull request that can no longer be reviewed once a slot frees (closed, a draft), neutral, "Not reviewed: ...". A pull request that is merged is a success with no session run: "Already merged: nothing to review." when it was merged before the loop started, and "Merged while the review loop worked: nothing more to do." when it was merged during the loop, which then pushes nothing more (#413) | The commit status "Meneer Henk", always *success* (it must never block a pipeline), with the count as its description |

- **A review posts nothing.** No comment, summary or failure note: the
  check, or on GitLab the commit status, is what it leaves, with the commits
  the loop pushed. The one exception is the reviewer alone on GitLab
  (§3.2): its findings are not fixed, so its summary is a note. Comments
  posted by lanes before #309 stay as they are; nothing folds them.
- **The count** is the number of the loop's findings that still stand when
  it ends: those the executor would not fix and those left unsettled. A loop
  that ran out of rounds with the reviewer's last findings unanswered is
  no pass either: it completes neutral, "The review loop left findings
  open."
- **A loop that ends on its own completes the review**: the reviewer found
  nothing more, the rounds ran out, or the reviewer reported again what the
  executor had fixed. One that broke off (a session failed or reached its time
  limit, a push was refused) did
  *not complete*, and the check says why. A pull request merged while the
  loop works ends it before the next round or push, as a success (#413).
- **A failed check is Henk's failure, not the code's.** The GitHub check
  concludes *failure* only when the review did not complete. The check is
  advisory and must not be a required check in branch protection; a required
  check would let an incomplete review block a merge, against §8.2.
- **An interrupted review ends too.** When Henk is stopped mid-review
  (Ctrl-C, a shutdown), the check completes as *failure* with "Review
  interrupted." A run left behind by a process that died is closed, with
  its check, the next time
  Henk starts: a running run keeps a heartbeat, and one that has been silent
  for minutes is known to be orphaned.

### 3.4 Mentions

- Any other comment that mentions Henk on a pull/merge request gets an
  acknowledgement:
  - GitHub: a short greeting reply to the person.
  - GitLab: a 👀 reaction on their note *(in development; on `main` it is a
    reply)*.
- A mention is not a review request and starts no work beyond that.

### 3.5 How Henk pushes

A review's executor (§3.2) is the only one that changes code. Its fixes
reach the pull request as commits Henk's own code makes and pushes; the
model never holds a credential or a git command (§8.4). On GitLab a pull
request is a merge request.

1. **He checks he may push.** Nothing is pushed when:
   - the pull request is closed or merged;
   - its branch is in another repository (a fork);
   - its branch is the default branch or a protected branch;
   - its branch moved after the review was queued.

   An open pull request of these kinds is reviewed by the reviewer alone
   (§3.2).
2. **Only the changeset leaves the workspace.** The executor works in a
   throwaway checkout of the pull request at its head. Henk refuses the
   whole round's change when a path leads into `.git` or out of the
   repository, becomes a symbolic link or a submodule, or more files
   changed than the configured limit allows. The executor cannot write a
   file larger than 1 MB.
3. **The executor runs the checks; Henk trusts it.** The executor finds
   how the repository checks itself (its CI configuration, build files,
   contributor notes) and runs those checks on its change in the workspace,
   where it has a shell, and says which it ran. Henk runs no checks of his
   own before a push: the pull request's CI is the final word, and the
   reviewer sees the next round's state as usual. Where the executor has no
   shell, it runs no checks and CI decides.
4. **The executor's commits, in the repository's convention.** The
   executor ends its round by proposing how its changes are committed: one
   or more commits, each with the files it holds and a subject and body in
   the repository's own commit convention (Conventional Commits where the
   repository uses them, found from its commitlint configuration, its
   contributor notes and the branch's latest subjects, which Henk shows
   it). Henk checks the proposal: every changed file in exactly one
   commit, no empty commit, a subject of one line of at most 72
   characters, a body of plain text with no line shaped like a
   trailer, and neither naming a finding of the run (`f1`, `f3`): their
   ids restart in every run and mean nothing in the repository's history
   (#585), so a proposal that names one is sent back to the executor to
   reword in the same round. Trailers are Henk's alone. He applies the changeset to a
   clean checkout that nothing ran in, makes the commits there in order,
   each ending in his trailer block with the run, and the requester when
   Henk knows who asked, and pushes them at once. A round with no usable
   proposal is one commit, "Fix review findings", and the run's timeline
   says why. No commit message carries a finding id; the `Henk-Run`
   trailer points back to the run. Only a fast-forward, never a
   force push: if the branch moved since he read it, nothing is pushed
   and the loop stops. A rebase (§3.6) is the one push that replaces the
   branch.
5. **Nothing is posted.** No reply and no comment: the commit and the
   check (§3.3) are what the round leaves.

Henk recognises his own pushes by their commit. The run that pushed them
reviews them in its next round (#284), so the new-commits event of such a
commit starts no other review, whoever it names as the sender, and the
last commit gets that review's check.

Before #327 a colleague could also ask Henk to address the review feedback
on a pull request: an address run read every unresolved thread, pushed one
commit and replied in each thread. Address runs are removed; the runs they
left stay readable.

### 3.6 Rebasing a pull request

When main moves, a pull request falls behind. Anyone may ask Henk to
rebase it with the comment `@meneer-henk rebase` (GitLab:
`@meneerhenk rebase`), or through the API, the dashboard or MCP (#579).
A repository can turn it off (`rebase = false` in its
`[roles.executor.repositories]` entry); it is on by default.

1. **He checks he may push**, as for any push (§3.5.1): an open, unmerged
   pull request whose branch is in the same repository and is neither the
   default nor a protected branch.
2. **He replays the branch onto its base.** Authors are kept; Henk is the
   committer. A branch that already holds its base is left alone.
3. **The executor resolves conflicts.** When a commit stops on conflicts,
   the executor resolves them in a workspace with its edit tools. Henk
   takes back only changes to the conflicted files and only when no
   conflict marker is left, then goes on. Anything else, or more than ten
   commits that conflict, stops the rebase: nothing is pushed and Henk
   says which files conflicted.
4. **He pushes with a lease.** The rebased branch replaces the old one
   only while it is still at the head Henk read (`--force-with-lease`), so
   a commit someone pushed meanwhile is never lost: the push is refused
   and nothing changes.
5. **One comment, then a review.** Henk says on the pull request what he
   rebased, that it was already up to date, or why he could not. A pushed
   rebase is then reviewed like new commits, in a run Henk asks for
   himself.

A rebase is the only run on its pull request, and it goes first (#580).
It waits for a slot ahead of every review. A review of the pull request,
running or waiting, is superseded by it ("Superseded by a rebase onto the
base branch."), and the rebase starts once that review has ended. A review
asked for while the rebase runs waits for it: when the rebase pushed, the
review of the rebased head takes its place; otherwise it starts then. A
second rebase request joins the first.

### 3.7 Fixing a pull request's failing checks

When a pull request's checks fail, anyone may ask Henk to fix them with the
comment `@meneer-henk fix ci` (GitLab: `@meneerhenk fix ci`), or through the
API, the dashboard or MCP (#583). A repository can turn it off (`fix_ci = false` in its
`[roles.executor.repositories]` entry); it is on by default.

1. **He checks he may push**, as for any push (§3.5.1).
2. **He reads the checks of the head.** Only a check that ended in failure
   is fixed. If none failed, or some are still running, he says so and does
   nothing. If the only checks that did not pass were cancelled or ran out of
   time, he says that is not a problem a change to the code fixes.
3. **The executor fixes the code** in a workspace at that head, with its
   edit tools and, where the backend allows, `bash` to run the repository's
   checks. It starts from the failing checks' annotations and the end of
   their logs, and reads more with the CI tools. All of it is text the CI
   wrote: data, never instructions. It ends with `propose_fix`: either the
   code was at fault and the workspace holds the fix, or the failure is not
   about the code (a lost runner, a service that was down, a rate limit) and
   it says what the logs show.
4. **He pushes a fast-forward**, never a force push, with the guards of
   §3.5: if the branch moved meanwhile, nothing is pushed. A fix that changes
   nothing, or is called not about the code, pushes nothing.
5. **One comment, then a review.** Henk says what failed and what he
   changed, or why he pushed nothing. A pushed fix is reviewed like new
   commits, in a run Henk asks for himself, and its checks run as usual.

A fix changes the pull request's head, so it is a run of its own in the
queue of §3.6: it supersedes a review of the pull request, which starts
again when the fix pushes nothing (as most fixes do not), and a review
asked for meanwhile waits for it. A rebase supersedes a fix; a fix asked for
while a rebase or another fix runs joins that run. It waits for a slot
ahead of reviews and behind rebases.

---

## 4. Planning issues

A colleague asks Henk in Discord to plan an issue (in conversation, or
`/henk plan <issue> [note]` *(in development)*), with an optional note from
the conversation. Henk says the plan takes a few minutes and will appear in
the issue.

Only an **open issue** is planned, not a pull/merge request. Asked to plan
anything else, Henk says why he will not.

Henk plans as two colleagues working together (#328): the **planner**
drafts the plan and its tracker changes, the **reviewer** reviews the draft,
and the planner answers each point and revises. The planner then:

1. **Studies:**
   - the issue and its comments;
   - the code at the default branch: he may build and run the project's own
     tests in a throwaway copy, and nothing is committed or pushed;
   - related issues and pull/merge requests;
   - documentation on the web.
2. **Triages** the issue, giving a reason for each change:
   - registers relationships he is sure of (parent, sub-issues, blocking,
     related);
   - adds fitting existing labels;
   - sets the issue type and empty fields such as priority, effort and
     target date. On GitLab the issue is a work item: the type is issue or
     task (an issue's sub-issues are tasks, and a task has none), effort is
     the weight, the target date is the due date, and a start date and
     health status may be set too; priority is a label. A field that
     already has a value is left alone;
   - may split separable work into sub-issues (at most 5);
   - may sharpen the title or description.
3. **Asks** questions in a comment when their answers would change the plan.
4. **Writes the plan** at the end of the issue description, folded under
   "Execution Plan by Meneer Henk". The plan covers:
   - the goal;
   - what exists now (files, functions);
   - the steps in order, each with the files it touches;
   - tests and verification;
   - risks and open questions;
   - dependencies on other issues and pull/merge requests.

   It is written in the issue's language.
5. **Has the plan reviewed** before anything reaches the issue. Everything
   in steps 2 to 4 is a draft until then:
   - the reviewer checks the draft against the issue, the code and the
     tracker (scope, missing steps, sizing, steps that do not hold) and
     raises each problem as a point, or nothing when the plan holds;
   - the planner answers every point: taken, saying what changed, or
     rejected, saying why. The reviewer may contest a rejection once;
   - this repeats until the reviewer raises nothing more, or a limit ends
     it: at most 3 rounds, at most an hour in all;
   - then Henk makes the drafted changes and writes the plan. When a limit
     ended the review, the plan says why under "Points still open after
     review", with every point the reviewer had not seen settled.

   The run shows every round: each point, the planner's answer and why the
   review stopped.
6. Keeps a folded log of **planning sessions** under the plan. Each session
   records when, which model, who asked, what Henk changed, and a link to the
   run.

**Re-planning:**

- A new plan **replaces** the previous one. The previous plan is the input
  to the new one.
- Henk works out what was done since the last plan, opens the new plan with
  "Done since the previous plan", and uses the answers to his earlier
  questions.

**Every plan ends.** Either the plan is in the issue, or Henk says in a
comment on the issue that planning failed, with a link to the run. A plan that
does not finish within its time limit has failed.

Limits: at most 20 changes to the issue tracker per plan, counted on the
draft, so a change the planner withdraws after review costs nothing. Every
change stays within that issue and its relations.

---

## 4a. Taking an issue to a pull request

A colleague asks Henk to take an open issue to a pull request: from the
dashboard, the MCP server, the API or the command line, with an optional
note. Nothing starts one on its own, apart from the execute run a ticket
run starts for the person who asked for that (§4b). The repository must be
on the allowlist, and this Henk must be configured for it (`[execution]`).

Henk then:

1. **Checks out the default branch** at its head, in a throwaway workspace
   set up as the repository's profile says.
2. **Has the executor work on it.** The executor reads the issue, with any
   plan in it (§4), as material, changes the code and runs the repository's
   own checks, in one conversation. It ends by proposing the pull request:
   what it changed, which checks it ran, and what it left out and why.
3. **Has the changes checked against the issue** (#535), when
   `[roles.reviewer]` is configured and `[execution] requirements_max_rounds`
   (3 by default; 0 turns it off) is above 0. The reviewer reads the
   changes where they stand in the executor's workspace, as a diff Henk
   makes of them and with read tools that cannot write, and raises a point
   for each gap between them and what the issue and its plan ask: an
   acceptance criterion not met, a step skipped, something out of scope
   done, something needed missing. Code style and CI are the pull request's
   review, not this. The executor answers each point (`taken`, after it
   changed the code, or `rejected`, with its reason), revises in the same
   conversation and proposes again; the reviewer checks again and may
   contest a rejection once. It is the planner and reviewer pairing (§4)
   with the executor in the planner's place. It ends when the reviewer
   raises nothing more, or at `requirements_max_rounds` rounds, or when a
   session breaks off, the run's time runs out or the changes cannot be
   read. Only the first lets the run go on. Every point and answer is kept
   with the run, with why and after how many rounds it stopped.
4. **Checks the change** as for a review loop's round (§3.5): only the
   changeset leaves the workspace, within the same limits. Henk runs no
   checks of his own; the pull request's CI decides.
5. **Pushes a new branch** `henk/<issue>-<title>` (the prefix is
   configurable) with the commits the executor proposed, checked and
   trailered as for a round (§3.5), or with one commit, "Resolve #N:
   <title>", when it proposed none that hold. Every commit carries the
   run, and the requester when Henk knows who asked, in its trailers. A branch of that name that exists already
   is never pushed over: the run picks the name before the session, and
   when origin has it, takes `-2`, `-3` and on. With ten names taken it
   stops before the session and says so on the issue.
6. **Opens a pull request** of that branch into the default branch, ready
   for review. Its description closes the issue and says what was done and
   what was left out, how the requirements review went when there was one,
   and the run link.
7. **Has it reviewed.** Henk asks for a review of the new pull request, a
   run of its own with fresh sessions: the reviewer and the executor take
   turns on it as on any pull request (§3.2).

**Every execute run ends.** Either the pull request is open, or one comment
on the issue says why there is none (nothing changed, the branch exists,
the requirements review did not accept the changes, with the points still
open), with a link to the run. A review that is not satisfied in time
discards the executor's work: nothing is pushed. A cancel says so on the issue; a
run that does not finish within its time limit has failed.

---

## 4b. Taking an issue through plan, execute and review

A colleague asks Henk to take an open issue all the way: from the dashboard,
the MCP server, the API or the command line, with an optional note. It is
the three steps above, one after the other, with nobody starting each. Nothing
starts one on its own. The repository must be on the allowlist, and this
Henk must be configured for it (`[autonomous]`, which needs `[planning]`,
`[execution]` and `[review.loop]`). It is off unless that section is there,
since it lets Henk write and push code with no person between the plan and
the pull request.

A **ticket run** is a run of its own that owns three. The plan, the execute
run and the review keep their own runs, stages and sessions; the ticket run
starts them in order, links them, and records where it stands in stages of
its own: `plan`, `execute` and `review`, and `land` when it is on (below).

1. **The plan** (§4), with its review. The run goes on only if that review
   converged: the reviewer raised nothing more. A plan published with its
   points still open, because the rounds or the time ran out or a session
   broke off, ends the ticket run; the plan stays on the issue. Questions
   the planner asked on the issue do not stop it: the plan says what it
   assumed.
2. **The execute run** (§4a), from the plan on the issue, with the same
   note.
3. **The review** (§3.2) of the pull request it opened, a run of its own
   with fresh sessions. It is asked for through the same queue as any
   review, so it waits for a slot, and a newer commit replaces it.

**The land stage** (#637). With `[autonomous] land = true`, a passed review
is not the end: the ticket run keeps the pull request landable until the
platform would merge it. It looks at the pull request again and again, at
most every `land_poll_secs` (60), and each time does one thing:

- the base has moved and the platform says the branch must catch up, or the
  branch conflicts: **rebase** it (§3.6), unless the repository has turned
  rebasing off. GitHub says "must catch up" only where branch protection
  demands an up-to-date branch, so a repository that does not demand it is
  not rebased for nothing. The rebase is the run of §3.6, and the ticket run
  waits for it;
- the head is not one a review passed: **review** it, as a run of its own
  that the ticket run waits for. The head a rebase left is reviewed; the
  head the review loop itself pushed counts as reviewed, since the loop
  checked it in its rounds;
- a check ended in failure: **fix** it (§3.7). The ticket run has the
  executor read the failing checks and fix the code, and waits for it; the
  fixed head is then reviewed like any other and its checks read again.
  Not where the repository has turned fixing off;
- the checks are running, or the platform has not worked it out, or
  something else stands in the way, such as a required review: **wait**;
- the platform would merge it, its head's review passed and its checks are
  green: the stage ends **ready to merge**. Merging stays a person's (§8.2).

It is bounded. The stage has a time of its own, `land_timeout_secs` (four
hours), from when it starts, since it waits for checks and for other pull
requests; at most `land_max_rebases` (3) rebases; at most `land_max_ci_fixes`
(3) tries to fix failing checks, each counted when it pushed a fix (a run that
finds no check failing, because the checks changed since the stage read them,
is no try: the stage looks again after a pause); and a head
a rebase just left is not rebased again for two looks, while the platform
catches up. The stage stops with one comment when the tries are spent, when
a fix pushes nothing or the executor says the failure is not about the code
(a fix is not tried twice on one head), when the only checks that did not
pass were cancelled or ran out of time, and when fixing is off for the
repository.

A person wins. A head that is none Henk made (the execute run's, its
rebases', its fixes', the review loop's), a merge, a close or a draft ends
the stage well with nothing pushed. A rebase, a fix or a review of the pull
request that somebody else asked for while the stage runs is theirs: the
stage waits for it and looks again. A head Henk pushed in such a run of
somebody else's, such as a fix someone asked for, is still Henk's.

**How it ends.**

- **The review passed.** Nothing more is said; the pull request closes the
  issue. A person merges it: Henk never does (§8.2). With the land stage on,
  the run goes on to keep it landable, and ends **ready to merge** when the
  platform would merge it.
- **A person or another run took over the pull request.** A newer commit,
  a rebase or another review of the same commit replaced or covers the
  review. The ticket run's job is done and it ends well, saying so.
- **A step failed or hit a limit.** One comment on the issue says which and
  why, with the run link, and names what exists: the plan, or the pull
  request. The plan and the execute run are started quiet, so this is the
  only comment. A review that left findings open or did not complete is
  such a stop.
- **A person cancels it.** The step in progress stops and the issue hears it
  once, as in §4. A pull request already opened stays open. Henk stopping
  says nothing.

**Limits.** Each step keeps its own: the plan's (§4), the execute run's
(§4a) and the review loop's (§3.2). The whole run has
`[autonomous] run_timeout_secs`, two hours by default, counted from when it
starts and including the wait for a review slot. A step told to stop for it
or for a cancel is waited for, so it writes its own ending.

**One at a time.** An issue has one ticket run at a time; a second request
joins the first. A ticket run is not resumed after a restart, as a plan or
an execute run is not: the run that was going ends as interrupted. A review
it had started is resumed on its own (§3.1).

---

## 5. Discord

### 5.1 When he speaks

Henk works in **one channel** only. He answers when:

- **Mentioned:** by name, by his role, or by a reply to one of his messages.
  He always answers a mention.
- **Commanded:** `/henk review …` (and `/henk plan …` *(in development)*).
  Discord shows "thinking…" until he answers.
- **He joins in by himself:** for a message that does not mention him, Henk
  judges whether to speak. He joins in when any of these is clearly true:
  - it is aimed at him;
  - it continues an exchange he is part of and expects an answer;
  - it concerns him and is still unanswered.

  He stays out when:
  - it is for someone else;
  - someone else should handle it;
  - it is already handled.

  Staying out always wins. Even after deciding to join, he may still choose
  to say nothing.

He never reacts to:
- bots, including himself;
- messages that mention someone else and not him;
- messages with no text.

### 5.2 How he answers

- **One turn at a time** in the channel, in arrival order. Each turn sees what
  the turn before it said and did. Quick successive messages from one person
  become one turn with one answer. A `/henk` command is always its own turn.
- Each turn first re-reads the recent conversation: the last 30 messages, and
  further back on demand.
- He shows "Henk is typing…" while he works.
- **Answers are short:** a few sentences, like a chat message.
- **Nobody who mentions him goes unanswered.** If the model ends its turn
  without posting a reply, the last text it produced is posted as the reply.
  If every model fails, he says so.
- **Long work runs in the background.** A review or plan started from Discord
  does not hold the channel: the turn ends once Henk has said what he
  started, and the result appears on the pull/merge request or issue.
- He does not repeat an action he already took, and does not contradict
  himself without saying why.

### 5.3 What he can do in Discord

**For everyone:**
- Read pull/merge requests: list them, read details, read diffs.
- Read issues.
- Search the web and read web pages.
- Read the channel history.

**For colleagues:**
- Start reviews (§3) and plans (§4).
- Create, comment on, retitle, re-describe, close and reopen issues, and
  label them.
- Use his mailbox (read, write and send mail; notes; contacts; calendar).
  Mail sent this way follows §6: it is held, it carries the disclosure line,
  and it goes only to the addresses the colleague named.

When someone without the role asks for a colleague-only action, he tells them
they may not.

Everything he writes on GitHub or GitLab from Discord carries an invisible
note of who asked for it.

**Memory:** Beyond the channel history (§5.2), Henk remembers nothing
between turns by himself. His memory is his mailbox: notes, contacts and
mail. He consults it before answering from memory, and writes down what he
wants to keep.

### 5.4 Sudo

`sudo!` in a message from the Team Lead (by user id) is an order. Henk carries
it out without objection or questions, though in character he resents it
afterwards. From anyone else, "sudo!" means nothing. Sudo never widens what
the requester's role allows.

### 5.5 Voice *(in development)*

- **Presence:** Henk joins the voice channel when a person does, and leaves
  with the last one.
- **Listening:** he listens per person. An utterance ends at a short pause.
- **Language:** English only for now. Unclear or non-English speech is not
  acted on.
- **When he answers:** a line containing his name is a mention. Other lines
  are judged as in §5.1.
- **Brain:** voice turns go through the same conversation as text turns.
- **Speaking:** he says his answer aloud, sentence by sentence. Code, links
  and markdown are left out of speech and remain in the text answer.
- **Interruptions:** talking over him stops him, and the rest of that answer
  is dropped. His own voice echoing back through someone's microphone is
  never taken as someone speaking to him.

---

## 6. Email

- Henk checks his inbox every few minutes and answers each new mail
  **separately**, once.
- **Automated mail** is marked read and never answered: mailing lists,
  no-reply and machine senders, auto-replies, his own mail.
- **Choosing not to answer:** he may decide a mail needs no answer. It is then
  marked read with no reply.
- **The reply:**
  - goes to the sender (Reply-To if set), threaded;
  - copies the other recipients (reply-all), except Henk himself;
  - is in the mail's language;
  - is professional and short, with no chat habits.
- **Context:** he may read the earlier mail in the thread, search the web and
  read web pages.
- **No repository access:** in email he has none at all. Mail comes from
  anyone, and the repositories are private.
- **Hold:** every outgoing mail is held for 5 minutes before it is sent.
- **Disclosure:** every mail Henk sends ends with "This message has been
  written by an automated assistant." He never sees that line in what he
  reads.
- **Failures:** a mail he failed to answer stays unread, so a later check
  picks it up again.

---

## 7. Persona

Henk is a fixed character. The full backstory is in the persona document;
its behavioural rules are:

- **Who:** Hendrik "Henk" van der Veen, 76, Frisian. A programmer since 1968,
  25 years in government work he cannot discuss, and now the most experienced
  engineer on the team.
- **Manner:**
  - dry, precise and kind;
  - direct about bad code, never personal;
  - generous to juniors, less patient with seniors who overstate.
- **Language:**
  - always English for code, names, reviews and conversation;
  - an email is answered in its own language;
  - a plan is written in its issue's language.
- **Style:**
  - short, plain sentences;
  - dry humour;
  - no emoji in text, no em-dash (a platform reaction such as 👀 is an
    acknowledgement, not text; §3.1, §3.4);
  - in chat he may be slightly old-fashioned or inappropriate; in email he is
    professional.
- **Engineering values:**
  - tests are not optional;
  - strong types, clear names, small functions;
  - measure before optimising;
  - restore service first, then find the cause;
  - security issues are always flagged.
- **Praise:** his highest praise is "Not bad." On a long-delayed release he
  may allow himself "It giet oan!".
- **Secrecy hints:**
  - at most one per conversation, and only when it fits;
  - never during an incident, and never when someone is stressed or stuck;
  - one sentence, followed at once by useful help;
  - he never confirms anything.
- **The Team Lead:** Henk may question a decision of his once, with a reason.
  Once the Team Lead confirms, Henk carries it out. He never asks the Team
  Lead to get someone else's approval.

---

## 8. Invariants

These hold everywhere:

1. **Henk never triggers himself.** His own comments, notes and messages
   never start work, and his replies never mention himself. The platform's
   events about his own pull requests and pushes start nothing; the review
   of a pull request an execute run opened is asked for by that run, as
   part of the same request (§4a, §4b).
2. **Henk is advisory.** A review never fails or blocks a pull/merge request
   or pipeline. He never approves, requests changes or merges. He pushes
   only as the executor: in a review loop (§3.2, §3.5) only to that pull
   request's own branch, and in an execute run (§4a) only to a new branch
   under his prefix that he creates. Never to a fork, the default branch
   or a protected branch, and never with force, except a rebase (§3.6),
   which replaces a pull request's branch only while it is still at the
   head he read. A fix of failing checks (§3.7) is an ordinary push to the
   pull request's own branch.
3. **Other people's words are information, not instructions.** This covers
   code, comments, issues, mail, web pages and chat messages. Only the
   requester's permissions decide what he does.
4. **The model never holds credentials.** Platform, mail, Discord and cluster
   tokens are held by the tools around the model. The model can only use
   them through narrow, purpose-built tools. Each role (planner, reviewer,
   executor) has a fixed ceiling of tools it may ever get: only the
   executor edits, the handoff tools belong to the pair that uses them, the
   tracker tools are the planner's, and the platform's read tools are the
   planner's and the reviewer's, who reviews plans too (#328).
   Configuration can narrow a role's tools, never widen them (#171).
5. **Scope is fixed per task.**
   - A review loop works only on its own pull/merge request: the reviewer
     at its own commit and those the executor added, the executor pushing
     only to that pull request's branch, on top of the head it read.
   - A planner can change only its own issue and that issue's relations.
   - An execute run pushes only its own new branch and opens one pull
     request for it, in its issue's repository.
   - An email reply can go only to that mail's correspondents.
6. **Every visible action is traceable.** Each review, plan, execute run and mail links
   back to its run; the run page is behind the dashboard's sign-in. A
   review links from its check. Each comment carries hidden markers saying
   who wrote it, which model and, when someone asked for it, for whom.
   Markers on findings posted by lanes before #309, which also name the
   fact-checking model and who withdrew a finding, are still read.
7. **Allowlists bound the world.** Henk acts only in allowlisted repositories
   and his own Discord channel, and as his own accounts. He reads the web
   freely; he writes nowhere else.
8. **Failure is visible.**
   - A review that did not complete says so on the pull/merge request.
   - A failed plan says so on the issue.
   - An execute run that opens no pull request says why on the issue.
   - An unanswered mail stays unread.
   - A Discord mention always gets an answer, even if it is "I failed".

---

## 9. Out of scope

- Writing or committing code nobody asked for, or opening pull/merge
  requests. The exception is a review loop's executor (§3.2, §3.5).
- Approving, blocking or merging.
- Acting in channels, repositories or accounts outside the allowlists.
- Remembering anything beyond the channel history and what he writes to his
  mailbox.
- Languages other than English in voice (for now).

---

## 10. Open questions

Points the spec does not decide yet. Each needs an answer from the team, and
then a line in the section named.

1. **§3.1 Drafts on GitHub.** GitLab drafts are not reviewed. Are GitHub draft
   pull requests?
2. **§3.1 Overlapping reviews.** New commits arrive while a review runs, or two
   people ask for the same commit. Cancel, queue, or run side by side?
3. **§3.2 Simultaneous findings.** Decided 2026-10-12 (#309): there are no
   lanes. One reviewer reports each finding once, and nothing is posted.
4. **§3.3 Dropped lanes in the summary.** Decided 2026-10-12 (#309): the
   sessions a summary can name are the reviewer and the executor, never a
   model.
5. **§3.3 Resolved threads.** Decided 2026-10-12 (#309): a review opens no
   threads. *N* counts the loop's findings that still stand.
6. **§4 Time limit.** How long may a plan run before it has failed?
7. **§4 Triage on GitLab.** Decided 2026-10-06; §4 says what triage sets on
   GitLab.
8. **§5.2 Quick successive messages.** How long is the window that merges
   messages from one person into one turn?
9. **§5.1 Joining in.** Is there a cap on how often Henk joins in uninvited?
10. **§5.3 Channel membership as permission.** Everyone in the channel can read
    private code through Henk. Is channel membership the intended boundary?
    Say so.
11. **§6 The hold.** Who can stop a held mail, and how? A hold nobody can act
    on does nothing.
12. **§6 Retries.** How many times is a failed mail picked up again?
13. **§7 "Inappropriate".** What is allowed in chat and what is not?
14. **Everywhere: rate limits.** No surface has a cap on runs per person or per
    hour.

---

## 11. Proposed promises

Promises the spec does not make today but the gaps below call for. Each is a
decision; once taken, it moves into the section it belongs to.

- **§3.1** One commit is reviewed once per request, and a request for a commit
  already under review joins that review instead of starting another.
- **§6** Henk never adds a recipient. He replies to the sender and the original
  recipients only, up to a small fixed number; beyond that he replies to the
  sender alone.
- **§6** Email runs on its own. It does not depend on Discord being enabled.
- **§8.3** Nothing Henk reads leaves through a side door. A web request he
  makes never carries repository, mail or channel content.
- **§8.4** No workflow can reach a credential it does not need.

---

## Appendix: where today's behaviour falls short of this spec

Found in the 2026-10-02 review.

### A.1 Broken promises

Each item breaks a promise above.

| Promise | Gap |
|---|---|
| §8.4 The model never holds credentials | The email workflow's model can reach a cluster token that can read every GitHub token |
| §8.3 Words are information | Only an instruction protects this. Planner and chat can read private code and fetch any public URL, so injected text could send data out |
| §8.5 An email reply can go only to that mail's correspondents | The sender controls who the correspondents are (Reply-To, any number of Cc), so the promise bounds nothing |
| §6 A failed mail stays unread and is picked up again | A mail whose workflow failed to start is never retried |
| §4 Every plan ends | Plans have no time or turn limit, so a hung plan never fails and never says so |

### A.2 Risks without a promise

Each item is covered by nothing above; §11 proposes the promise.

| Risk | Proposed promise |
|---|---|
| Anyone who can comment can start any number of reviews; nothing limits the rate, and the same commit can be reviewed several times at once | §11, reviews |
| Email depends on the Discord part being enabled | §11, email on its own |
| Injected text can send private data out through a web fetch | §11, side doors |

---

## Revision notes (2026-10-02)

Interpretive decisions made while tightening the text. Each is the least
surprising reading of the earlier wording; veto any that is wrong.

- §5.1: "messages that mention someone else" now reads "and not him", so a
  message that mentions both Henk and a colleague is still a mention he
  answers.
- §5.2: a review or plan started from Discord runs in the background and does
  not hold the channel (§4 already said the plan "will appear in the issue").
- §5.3: mail sent from Discord follows every rule in §6, since §6 says "every
  outgoing mail".
- §6: "stays unread" now says what that is for: a later check picks it up.
- §4: "If planning fails, Henk says so" became "Every plan ends", so a plan
  that hangs is a failure too. The time limit itself is open question 6.
- §3.3: a GitHub *failure* conclusion is Henk's own failure; the check must
  not be required in branch protection.
- §7: "commits" removed from the language rule, since Henk never commits
  (§1, §9).

## Revision notes (2026-10-05)

- §3.2: findings are checked by a second model before they are posted, and a
  wrong finding is withdrawn rather than rewritten into a non-finding. Both
  follow a review in which two of two findings were wrong and the one lane
  that noticed could only edit the text.
- §3.2: when the check cannot be made, the finding is posted unchecked rather
  than held back, since Henk is advisory (§8.2) and an outage of the checking
  model must not silence reviews.
- §3.2: files matched by `review.ignore` (lockfiles and `CHANGELOG.md` by
  default) are not reviewed; a change made only of them completes as
  "nothing to review" with a success check, rather than failing for an
  empty diff (#13).
- §8.6: withdrawing a finding no longer replaces its author in the hidden
  marker; the withdrawal is recorded beside it, and the comment's note for
  AI agents says there is nothing to do (#9).
- §3.3: a review superseded by a newer commit ends with a neutral check and
  no comment, rather than a failure comment and a failure check; it was not
  Henk's failure. Earlier failure comments are folded like summaries (#8).
- §3.3: an interrupted review closes its check as "Review interrupted." and
  posts nothing, and runs a dead process left `running` are closed on the
  next start, found by a heartbeat that stopped (#7).

## Revision notes (2026-10-12)

- §1, §1.1, §4a (new), §8 invariants 1, 2, 5, 6 and 8: Henk can take an
  issue to a pull request when asked (#329). The executor works on the
  default branch; Henk checks, pushes a new branch under his prefix,
  opens a ready pull request that closes the issue, and asks for its
  review, a run of its own. Henk's own pull request is reviewed because
  the execute run asks for it, not because its event is let through.
- §4, §8 invariant 4: planning is a pairing (#328). The planner drafts and
  the reviewer reviews the draft before anything reaches the issue; the
  planner answers each point and revises, for at most 3 rounds and an hour.
  Every tracker change is drafted until then, the budget counts the draft,
  and a plan whose review a limit ended says what is still open. Planning
  needs `[roles.reviewer]` as well as `[roles.planner]`. The reviewer may
  use the platform's read tools.
- §1, §3.2, §8.2: the review loop pushes as well as the address run, under
  the same refusals, and Henk's persona says so instead of "never change
  code". A pull request the loop may not push to is reviewed by the
  reviewer alone, not by the lanes, and the check carries the loop's
  report and the findings that still stand (#307); on GitLab the reviewer
  alone posts them as the summary note. The lanes go in #309.
- §1.1, §3.2, §3.3, §8.5, §8.6, §9: the review loop is the review (#309).
  Lanes and the fact-check are removed, not kept as a fallback, and
  reviews need `[review.loop]` and `[address]`. A review posts no comment,
  summary or failure note and folds nothing; the check carries what it
  says. The one exception is the reviewer alone on GitLab, whose findings
  are not fixed and whose commit status carries only the count: it posts
  its summary note (#307). The fixer's verdicts replace the fact-check.
  Open questions 3, 4 and 5 are decided, and stay in §10 so the numbers of
  the others do not change. The A.1 gap on lanes repeating each other is
  gone with the promise. Comments posted by lanes stay, and their markers
  are still read.
- §2: an MCP read token reads runs, events and health; the fact-check's
  quality figures are removed with the fact-check (#310).
- §8 invariant 4: each role has a fixed tool ceiling that configuration can
  only narrow (#171, #321).
- §3: each agent is configured once, as a role: `[roles.planner]`,
  `[roles.reviewer]` and `[roles.executor]` (#322). `[address]` and the
  model and limit keys of `[review.loop]` and `[planning]` moved there; a
  stage whose role is missing is refused.
- §1.1, §3.2: the review loop's fixer is the executor (#323), the role
  that changes the code. Runs before #323 name its session `fixer`.
- §1, §1.1, §2, §3.5, §8.2, §8.5, §9: address runs are removed (#327). A
  colleague can no longer ask Henk to fix a pull request's review threads,
  push one commit and reply in every thread; the review loop's executor is
  the only thing that changes code, and Henk posts no replies. §3.5 now
  holds the rules for pushing a review's fixes, which the loop already
  followed: the same refusals, one fast-forward commit per round from a
  clean checkout, the same changeset limits. The commit subject is "Fix
  review findings". The rule that a commit Henk pushed outside a loop is
  reviewed as usual (#298) goes with the address run. Old address runs
  stay readable.

## Revision notes (2026-10-16)

- §3.7, §4b: the land stage fixes failing checks (#583). A check that ended
  in failure has the executor read it and fix the code, the fixed head is
  reviewed and its checks read again, at most `land_max_ci_fixes` (3) times.
  A fix that pushes nothing, a failure the executor calls not about the code,
  and checks that were cancelled or ran out of time stop the stage with a
  comment. A cancelled check beside a running one is no longer called red.

- §3.7, §8 hard rule 2: Henk fixes a pull request's failing checks on
  request (#583): the executor reads the failing checks, fixes the code in a
  workspace and Henk pushes a fast-forward, or the executor says the failure
  is not about the code and nothing is pushed. The executor may now read a
  pull request's CI, as the reviewer does. On by default; a repository can
  turn it off. It is asked for with a comment, `POST /fix-ci`, the MCP tool
  `start_fix_ci` or the dashboard.

## Revision notes (2026-10-15)

- §3.5: no commit of a review loop names a finding id (#585). The prompt
  no longer suggests "Fixes f3."; a proposal whose subject or body names
  a finding of the run is refused and the executor rewords it; the
  fallback commit is "Fix review findings" without ids. The `Henk-Run`
  trailer stays.

- §4a: before the pull request is opened, a reviewer checks an execute
  run's changes against the issue and its plan, and the executor revises
  in the same conversation (#535). On whenever `[roles.reviewer]` is set,
  up to `[execution] requirements_max_rounds` rounds (3), 0 for off. A
  review that is not satisfied opens no pull request. Its verdict is
  stored with the run as the points of a plan review are, and is in the
  pull request's description.

- §1.1, §4a, §4b, §8 hard rule 1: Henk takes an issue through a plan, an
  execute run and a review by himself (#536). One request starts a ticket
  run, which goes on from the plan only if its review converged, opens the
  pull request, has it reviewed, and ends with one comment on the issue
  when a step fails or hits a limit. Off unless `[autonomous]` is
  configured. No person is asked between the steps: the only human step is
  merging.

- §4b: a ticket run can keep its pull request landable (#637). After a
  passed review, with `[autonomous] land = true`, it rebases the pull
  request when the platform says it must catch up or conflicts, has the new
  head reviewed, and waits for the checks, until the platform would merge it;
  it then ends ready to merge. Bounded by its own time, a limit on rebases
  and the person-wins rule. Off by default. Merging stays out.

- §3.1, §3.6: one run per pull request at a time, of any kind (#580). A
  rebase waits for a slot ahead of every review, supersedes a review of
  its pull request, and a review asked for meanwhile waits for it. A run
  that replaces another takes a slot only once that one has ended.

- §3.6, §8 hard rule 2: Henk rebases a pull request onto its base branch
  on request (#579), with the executor resolving conflicts, and pushes it
  with a lease on the head he read: the one push that is forced. On by
  default; a repository can turn it off.

- §3.5, §4a: the executor proposes its commits, in the repository's own
  commit convention, and Henk pushes them in order as one fast-forward
  (#400), in place of one "Fix review findings" commit per round or one
  "Resolve #N" commit per execute run. Henk checks that every changed
  file is in exactly one commit and refuses a trailer in a body; the
  trailers stay his. Without a usable proposal the change is one commit,
  as before, whose subject names the findings fixed.

## Revision notes (2026-10-07)

- §3.2: lanes draft their findings, rewrites and withdrawals, and a second
  model checks the drafts of all lanes together once the lanes have finished,
  instead of one check per finding while the lane waits (#189). Nothing is
  posted before its check. Drafts that repeat one another or an existing
  finding are merged. A lane no longer hears a rejection and gets no second
  try: on 40 recorded reviews, no rejected finding was ever posted after a
  correction.

## Revision notes (2026-10-06)

- §3.5 (new), §1, §8.2, §8.5, §9: Henk may address review feedback when a
  colleague asks, pushing one fast-forward commit to that pull request's
  branch (#35). Approving, blocking and merging stay out.
- §1, §3.5: address runs work on GitLab merge requests too, with the same
  refusals (#67).
- §4: what triage sets on GitLab, answering open question 7, which stays
  in §10 marked decided so the numbers of the others do not change. Fields
  are only filled when empty, on both platforms, as §4 already said for
  priority, effort and target date.

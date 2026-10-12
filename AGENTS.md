# AGENTS.md

## CI merge queue and runner capacity

The merge queue runs on a small pool of self-hosted runners (one or two) against a large job
backlog. A `merge_group` check that sits `in_progress` for a long time is almost always waiting
for runner capacity, not hung. Do not cancel merge-queue runs or dequeue PRs on the assumption
that the queue is stuck: a queued PR can take a long time to register and merge. Wait it out, or
admin-merge if a bypass is genuinely needed.

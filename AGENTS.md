# AGENTS.md

## CI merge queue

Nothing reaches `main` without the merge queue's mandatory gate: it dispatches
hoprnet/hopr-integration-tests and blocks on the v5 throughput + PIX verdict for
the candidate (`.github/workflows/merge-queue.yaml`), a suite that runs up to
~170 minutes by design. The heavier queue jobs also share a small pool of
runners against a large backlog. So a `merge_group` check that sits
`in_progress` for a long time, often hours, is expected rather than hung: it is
waiting for the long-running suite or for runner capacity.

Do not cancel merge-queue runs or dequeue PRs assuming the queue is stuck; wait
it out. An admin merge bypasses this gate and skips the mandatory
integration-test verdict, so treat it as a deliberate maintainer decision, not a
routine shortcut.

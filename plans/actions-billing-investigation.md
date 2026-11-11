# BootBob GitHub Actions cost investigation

## Goal

Find out why cinderblock/BootBob looked like the biggest GitHub Actions cost on the
account, and whether anything should change before a $20/month hard-stop Actions budget
goes on. Read-only: no workflow, settings, artifact or cache changes without a per-change yes.

## Environment / context

- Account `cinderblock`, plan `free` (from `gh api users/cinderblock --jq .plan`).
- BootBob is **public**, created 2026-10-23 4:10 PM PDT (23:10Z). No `PublicEvent` in its
  event feed, so it has been public since it was created.
- Billing endpoint: `gh api users/cinderblock/settings/billing/usage[?year=&month=]`.
- Snapshot taken 2026-11-10 ~5 PM PST (2026-11-11 ~01:00Z). Raw JSON saved in the session
  scratchpad (not durable).

## Findings

### 1. The $8.63 / 7,775 min is not BootBob's. It's the whole account's November Linux line.

- The endpoint **without** query params returns one row per *month × SKU* (56 rows,
  2026-01 → 2026-11), each stamped with a single `repositoryName`. The name is just one
  repo from that bucket, not the owner of the total.
- Proof: summing the `?year=2026&month=11` per-repo rows by SKU reproduces the
  no-param rows exactly:
  - Linux 7,775 min, gross $46.65, net $8.634 → labelled "BootBob"
  - Windows 1,317 min, net $2.86 → labelled "OpenSlate"
  - macOS 3-core 536 min, net $3.782 → labelled "open-task"
  - storage 5,404.8 GB-h → labelled "openslate-source-captures"
- Similar mislabels in older months (e.g. Sept Linux "libCameron", Oct Linux "svdsa.org").
- **Never read per-repo cost from the no-param view.** Use `?year=&month=` (per-day,
  per-repo rows).

### 2. BootBob's real cost: $0 net. Every row has discountAmount == grossAmount.

Oct 23 – Nov 10 (all `netAmount` 0.0):

- Actions Linux (`$0.006/min`): 533 (Oct) + 345 (Nov) = 878 min, gross $5.27
- Actions Linux ARM (`$0.005/min`, standard `ubuntu-24.04-arm`, not a larger runner): 60 + 172 = 232 min, gross $1.16
- Actions storage (`$0.00033602/GB-h`): 120 + 2,011 GB-h, gross $0.72
- No larger-runner SKUs, no macOS/Windows.

"List price" if it were private: roughly $9–12/month in minutes plus ~$3/month storage at
current activity. That is the only scenario in which it costs money.

### 3. What drives BootBob's (free) usage

- **CI** (every push to master + PRs): 6 parallel jobs (backend, web, ipxe, e2e, docker,
  publish). ~7 min wall, ~25–30 billed min per run. 52 runs since Oct 23. The bulk of the
  Linux minutes.
- **PC golden image** (amd64): ~20–26 min per run, push on path filter + weekly cron
  (Mon 9:47 UTC) + dispatch. 8 runs.
- **Pi golden image** / **Pi OS Lite golden image** (ARM): ~5–13 min per run. All the
  ARM minutes. Already path-filtered and weekly.
- **Storage growth** (120 → ~290 GB-h/day): golden-image artifacts, 0.27–0.83 GB each,
  30-day retention, `compression-level: 0` (images are pre-zstd'd). 12.6 GB of live
  artifacts now. CI artifacts (`bootbob-debug`, `ipxe`, buildx `.dockerbuild`) are tiny but
  kept 90 days (repo default). Caches: 2.54 GB / 71 entries (buildx `type=gha,mode=max` +
  rust-cache), which doesn't bill.

### 4. Who actually spent the ~$15 net in November (as of the snapshot)

- `apitorrent` (private, **created 2026-11-10 11:59 AM PST (19:59Z)**): $8.84 net in about
  five hours (macOS $3.78, Windows $2.86, Linux $2.20), with zero discount. This is the
  real risk to a $20 hard stop.
  Re-checked at the user's question: it really is private (`visibility: private`, no
  `PublicEvent`, `gh repo list` says PRIVATE). 19 CI runs by 4:13 PM PST (00:13Z): 8
  failed and 3 were cancelled, each running a Linux + Windows + macOS matrix. The Nov 10
  (UTC) billing rows show 348 Linux, 265 Windows and 56 macOS minutes.
  Created and pushed by the "Deluge Replacement Torrent Client" T3 thread
  (`3ddd5b82-6d22-4463-a879-2d9d9470899d`) on 2026-11-10 at 11:59 AM PST (19:59Z). It
  read the user's "at your leisure" reply to its "Creating the GitHub remote" item as a
  go-ahead. The user says that wasn't authorization; their intent is a public repo.
- `ops` (private): $6.10 net (2,757 Linux min, $10.44 of it covered by included minutes).
- `gate-manager` (private): $0.34.
- Total ≈ $15.28, which matches the "~$15 for Nov 1–10" figure.

### 5. Hard stop interaction

Standard runners on public repos don't draw on the paid budget (net is always $0), so
BootBob can't trip or crowd the $20 stop. To my understanding, a stop-usage budget only
blocks jobs that would incur paid usage, so BootBob jobs should keep running after it
trips (I haven't verified that empirically).

## Decisions already made (don't re-ask)

- Read-only investigation. No changes without a per-change yes.
- Don't touch ops.

## Recommendation

No cost-motivated change to BootBob. Every option below saves $0 in real money.
Optional hygiene only:

1. `retention-days: 7` on CI's `bootbob-debug` / `ipxe` uploads (they're only consumed
   within the run). Shrinks the artifact list, $0.
2. Golden images: keep 30 days (overlays tie to an exact disk, and older images may be
   needed). Could drop to 14 if the weekly cadence is enough. $0.
3. `concurrency: cancel-in-progress` on CI for PRs. Saves free minutes only.

The account-level action is elsewhere: look at `apitorrent`'s workflows (macOS/Windows
matrix on a private repo) before turning on the $20 stop. That's a separate thread.

## Progress log

- [x] Pull billing usage (no-param, Oct, Nov) and reconcile
- [x] Repo visibility and plan check
- [x] Read the 4 workflows; list artifacts, caches, retention, run durations
- [x] Report findings and recommendation to user
- [x] User said yes to the tidy-ups (2026-11-10): `c6172f3` sets `retention-days: 7` on
  `bootbob-debug` / `ipxe`, `DOCKER_BUILD_RECORD_RETENTION_DAYS: 7` on the publish build,
  and cancels superseded PR runs (master pushes are never cancelled). Applies to new
  artifacts only. Existing ones keep their 90-day expiry; none were deleted.
- [x] apitorrent dropped from this thread at the user's request.

## Open questions for the user

None.

## Things not to do

- Don't attribute cost from the no-param billing endpoint: it labels month×SKU totals
  with an arbitrary repo.
- Don't delete BootBob golden-image artifacts to "save money". They're free, and
  machines' overlays may depend on a specific dated image.

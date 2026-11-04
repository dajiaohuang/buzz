# Private-channel media revocation

`BUZZ_MEDIA_READ_MODE` is a startup setting: `off` (default), `shadow`, or
`enforce`. Unknown values fail startup. Channel revocation is effective only
in `enforce`. Blossom authentication, tenant binding, and community admission
remain mandatory in every mode.

## Read policy

Every GET and HEAD, including each Range request, authorizes the parent hash
before returning blob bytes, thumbnail bytes, existence, size, or range errors.
A tenant sidecar lookup precedes the new database lookup. Authorization uses
a fresh primary-database repeatable-read snapshot, without membership caches
or replicas. A removal committed before that snapshot excludes the member.
A request authorized before removal may finish afterward.

A file is readable through any current readable reference. Shared bytes may
therefore remain readable through another open channel or a DM the person
still belongs to. Channel owners/admins have no blanket exception to private
channel membership. Ordinary REQ event visibility, recipient-gated kinds and
encrypted engrams retain their restrictions. Gift-wrap ciphertext itself
cannot supply hidden attachment references.

Counted references are imeta `url` and `image` fields, profile `picture` and
`banner` fields, group metadata `picture` tags, and emoji tags on emoji lists
and sets. Root-relative paths and the tenant's HTTP/HTTPS media bases count.
A pasted URL in free text alone grants nothing; messages must publish imeta.
Deleted events/channels grant nothing. Only the newest live edit supplies a
message's attachments; its original must remain live. Superseded profiles
are excluded. Current community icons grant community-wide access.

The read path probes the existing tags GIN index for imeta hashes and exact
local URL aliases. It scans content only for current kind-0 profiles in the
community, never all message bodies. A two-second statement deadline and a
1,024-candidate budget per probe and a 16 MiB aggregate work budget bound each lookup. An accessible candidate can succeed
before an overflow refusal. Candidates stream one at a time rather than allocating an entire page of message bodies. Budget exhaustion or a database failure returns
503; an inaccessible/missing reference returns 404. NIP-FI strict mode keeps
its existing authorization-unavailable response contract.

## Uploads and publication

After the upload pipeline verifies the supplied bytes, the relay records
`media_uploaders` through community write admission. This is independent of
optional audit/upload moderation records. An unposted file is previewable by
its uploader only. Real reuploads may prove possession for another uploader.
Possession authorizes publication; a known URL/hash alone does not. A registered
agent must hold its own upload proof or readable reference; owner proof is never inherited.

The shared publication check runs before common ingestion dispatch (WS and
HTTP, including metadata commands) and before direct workflow persistence.
It uses the same reference extractor as reads, plus structured command URLs.
A durable `media_publications` marker consumes preview authority before
publication dispatch and survives source deletion. A later rejected operation
may conservatively consume a preview; reupload still proves publication
rights. Reuploads also record a boundary when a legacy reference already exists, so later purging that reference cannot revive previews. The marker grants no download access. Both tables are purged during
community deletion, and every writer uses admission.

Deployment-admin feedback attachment reads retain their independent admin
and feedback-row provenance checks. Feedback does not grant community-wide
media access through the ordinary media endpoint.

## Rollout and recovery

1. Apply migration 0083 (or desired-state schema) before enabling recording.
   It adds plain tenant tables, with no triggers, foreign keys or data rewrite.
2. Deploy the new binary on every writer with mode `off`.
3. Restart all writers in `shadow`; inspect
   `buzz_media_read_access_total{mode="shadow",reason="no_ref"}`,
   `reason="not_member"`, and `reason="lookup_error"`, plus
   `buzz_media_publication_access_total`. Exercise avatars, icons, emoji,
   uploads, private messages and old attachments during a staging soak.
4. Repair unsupported/historical URLs or republish them through an authorized
   uploader before restarting every serving pod in `enforce`.

There is no grant backfill: existing stored references are evaluated directly,
and signed event tags are never rewritten. Legacy unreferenced/orphan blobs
have no upload proof and are denied in enforce mode; their owner must reupload.
Pre-enforcement references remain authoritative, including a preexisting avatar
that points to the same bytes as a private attachment. Custom historical media
hosts beyond the tenant's current authority require explicit repair.

Roll back policy by restarting with `off`; retain the additive schema and
publication markers. Mixed off/enforce serving pods cannot provide revocation.
Shadow/off do not close the original exposure. Shadow lookup failures are
counted without refusing reads; enforcement fails closed.

Successful media responses and media denials use `private, no-store`. New
responses therefore increase repeat-download bandwidth. The desktop proxy
passes this policy to the WebView; its local file cache and the mobile client's
local copies can still display previously fetched bytes. Old immutable HTTP
cache entries, files already downloaded, screenshots and copied bytes cannot
be recalled. The guarantee applies to requests reaching the relay; migrating
existing client caches is a separate client rollout concern.

Any authorized member may deliberately republish or reupload bytes into a
readable channel or avatar. Revoking one channel cannot revoke that independent
share. This policy does not prevent authorized recipients from copying data.

Media authorization uses a shared semaphore across database-handle clones,
limited to one quarter of the writer pool, capped at eight lookups. Saturation
returns an authorization-unavailable error before pool acquisition. Every Range
request reauthorizes; large emoji sets have no 64-reference cap and checks run
sequentially within that connection bound. Missing sidecars reject publication
in enforce mode; shadow counts failures and accepts the write, including stale
avatar metadata edits and upload-proof/publication-marker errors.

During staging soak, abort enforcement if writer authorization acquire p95
exceeds 50 ms for five minutes, event-write acquire p95 doubles its pre-rollout
baseline, or media lookup_error/capacity refusal exceeds 0.1% of requests for
five minutes. Investigate query latency, pool utilization and storage errors
before retrying. Shadow failures must be repaired before enforce, since a failed
recording does not establish possession. Uploads made in off mode record no
proof: uploads not yet posted when enforce starts require reuploading.

New managed-agent creation explicitly downloads the chosen target-local avatar
with the captured owner's signer, verifies its image bytes and hash, then uploads
those bytes with the new agent's signer. The agent holds its own possession
proof. The captured community stays pinned through profile publication, even
if the owner changes workspaces during transfer. Automatic reconciliation never
borrows the owner's proof or signer.

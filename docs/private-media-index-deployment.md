# Media edit target index deployment

Before deploying migration 0085, applying desired-state schema to an existing
database, or enabling shadow, prebuild the partial btree
on the normalized first valid edit target. Signed event tags are never rewritten.
The immutable built-in JSONPath expression preserves tag order and hex casing.
No SQL function is added.

Use the same prebuild-then-verify approach as [thread windows](thread-window-deployment.md).
Run from this checkout, outside a transaction:

```sh
psql "$DATABASE_URL" -X -v ON_ERROR_STOP=1 -f scripts/maintenance/prebuild-media-edit-index.sql
```

The script creates an `ON ONLY` parent, builds each immediate events partition
index `CONCURRENTLY` with PostgreSQL-generated names (matching pgschema), then
attaches it. pgschema cannot build a partitioned parent concurrently, so this
prebuild is required for existing databases under either schema deployment path. The final bounded migration guard checks
readiness, validity, access method, key order, predicate and exact expression.
A valid parent proves every existing partition has an attached matching index.
New monthly partitions inherit the index through ordinary `PARTITION OF` creation;
the PostgreSQL regression creates a future partition and verifies attachment.
Keep partition management paused while running the prebuild; if a new partition
appears before verification, rerun the idempotent script and verify again.

A failed concurrent child build leaves an invalid index. Rerunning skips only
valid, ready, live matching indexes, so an invalid leftover does not prevent a
fresh concurrent build. After successful attachment, identify only the unused,
unattached invalid child whose definition matches the parent and drop it with
`DROP INDEX CONCURRENTLY` outside a transaction. Do not drop valid attached
indexes or replace concurrent builds with a blocking build. Wrong definitions
fail attachment or migration verification and need operator correction before
proceeding.

Migration 0085 builds directly only on an empty events table. Populated databases
without the prebuilt index fail startup with an explicit prebuild instruction.
With auto-migration disabled, operators must apply and verify 0085 before deploying
this binary, even in off mode. Bypassing that ordering and enabling enforce can
cause lookup timeouts and authorization-unavailable responses; restore off and
complete prebuild/verification before shadow. Do not use that bypass as a soak.

This is an additive expand/deploy change: existing relay reads and writes ignore
the new index, which adds only normal index maintenance. No row backfill, trigger,
function or foreign key is added. New and old binaries may serve against the
expanded schema. Keep auto-migration disabled on old binaries/restarts/rollback
because sqlx rejects unknown migration versions. Do not run community deletion
between 0084's table expansion and the new binary's deletion-catalog deployment.

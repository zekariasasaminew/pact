# Pattern: N call sites moving off a deprecated API

Good `spawn-many` candidate when you have a known-deprecated function/library
with several call sites that don't call each other. Split by call site (or by
directory/module), not by "migrate half the codebase" — a task an agent can
actually finish and verify on its own, with the concrete before/after
signature spelled out so every agent applies the same migration consistently.

```
pact spawn-many \
  --task claude:"In src/api/legacyClient.ts's callers under src/services/billing/, replace legacyClient.fetch(url, cb) with the new httpClient.get(url) (returns a Promise, no callback). Update tests to await instead of using the callback" \
  --task claude:"In src/api/legacyClient.ts's callers under src/services/notifications/, replace legacyClient.fetch(url, cb) with the new httpClient.get(url) (returns a Promise, no callback). Update tests to await instead of using the callback" \
  --task claude:"In src/api/legacyClient.ts's callers under src/services/analytics/, replace legacyClient.fetch(url, cb) with the new httpClient.get(url) (returns a Promise, no callback). Update tests to await instead of using the callback"
```

Once every call site is migrated and `pact merge-all` lands cleanly, deleting
`legacyClient.ts` itself is a good candidate for a single follow-up `spawn`
(not `spawn-many` — there's only one file to delete and one thing to check:
that nothing still imports it).

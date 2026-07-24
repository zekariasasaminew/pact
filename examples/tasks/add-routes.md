# Pattern: N new, independent routes/endpoints

Good `spawn-many` candidate when each new route is self-contained — new file
or a clearly separate block in an existing router, no shared state between
them, no route depending on another route existing first. If two routes need
to touch the same file (e.g. all registered in one `routes.rs`/`urls.py`),
give the shared-registration edit to a single agent instead, or expect a
`--union` merge on that one file (see `merge-all --union` in the README).

```
pact spawn-many \
  --task claude:"Add a GET /api/users/:id/orders endpoint that returns the user's order history, paginated, with tests" \
  --task claude:"Add a GET /api/users/:id/preferences endpoint that returns the user's saved preferences, with tests" \
  --task claude:"Add a DELETE /api/users/:id/sessions endpoint that revokes all active sessions for that user, with tests"
```

Follow up with:

```
pact merge-all --require-passing-tests "npm test"
```

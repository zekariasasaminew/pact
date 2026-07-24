# Pattern: the same mechanical change across N files

Good `spawn-many` candidate when the change is the same *shape* in each file
but the files themselves don't import or depend on each other — e.g. adopting
a new logging call, renaming a locally-scoped variable, switching a component
from class syntax to hooks. Give each agent the exact rule to apply plus its
one target file, not a vague "refactor the codebase" — a precise, narrow task
per file is what makes the parallel split safe in the first place.

```
pact spawn-many \
  --task claude:"In src/components/Header.tsx, convert the class component to a function component using hooks. Keep behavior identical, keep existing tests passing" \
  --task claude:"In src/components/Sidebar.tsx, convert the class component to a function component using hooks. Keep behavior identical, keep existing tests passing" \
  --task claude:"In src/components/Footer.tsx, convert the class component to a function component using hooks. Keep behavior identical, keep existing tests passing"
```

If the files share an import (e.g. a common `withLegacyLifecycle` HOC being
removed everywhere), mention that explicitly in each task so every agent
handles it the same way rather than guessing independently.

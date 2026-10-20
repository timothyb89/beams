
## Teleport AI use policy

Note that Teleport's AI use policy requires that no AI written code is
committed to production repos, at least outside of limited use in tests. This
applies to repos under `github.com/gravitational/` including Teleport clones in
`~/projects/teleport` and `~/projects/teleport-*`, and other projects outside of
the Teleport monorepo like `docs-website`, `cloud`, and others.

Where this policy applies, direct edits to files should be avoided unless
explicitly requested. Outlines, inline code snippets, and detailed explanations
are the preferred starting point. For more in depth conversations, creating a
temporary Git worktree off the relevant project or repo clone is preferred for
building, testing, or iterating on solutions; full write permission can be
granted in the worktree so the diff can be examined in depth. 

Personal projects and tooling (under the `github.com/timothyb89` namespace, etc)
do not have this restriction. This includes `teleport-harness`, `alt-drag`, etc.


## SSH keys on a Beam

On a Beam, `SSH_AUTH_SOCK` is a relay: the private key stays on the user's
workstation, and the user approves each use of it in their terminal. Before
running anything that uses the key (`git clone`/`fetch`/`push` over SSH,
`ssh` to a host, SSH commit signing), set `SSH_KEY_REASON` to a short,
honest, one-line reason; it's shown in the approval prompt:

```sh
SSH_KEY_REASON="Fetch the PR branch the user asked me to review" git fetch origin pull/123/head
```

Expect the command to wait (up to a minute) for approval. A failure such as
`agent refused operation` or `Permission denied (publickey)` means the user
declined or isn't at their terminal: don't retry in a loop or look for
other keys or agents; tell the user what you needed and why.

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

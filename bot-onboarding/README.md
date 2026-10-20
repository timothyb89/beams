# bot-onboarding

Cluster-level setup for the `k3s-bot` used by `bootstrap`'s `k3s-bot-join`
step. Applied **once**, cluster-wide — not per-beam, and not run by
`beam-bootstrap` itself.

```sh
tctl create -f role-k3s-bot.yaml
tctl create -f bot-k3s-bot.yaml
```

(`tctl` needs `TELEPORT_HOME=~/.tsh` and `--auth-server=$TELEPORT_PROXY` to
use your logged-in `tsh` session if there's no local Auth Service.)

- `role-k3s-bot.yaml` — grants access to the `k3s` Kubernetes cluster
  (`kubernetes_groups: [system:masters]`) and SSH access to its `workstation`
  host node (`logins: [core]`). Already verified end-to-end: a bot instance
  joined with this role can both `kubectl get nodes` through the generated
  kubeconfig and `tsh ssh core@workstation`.
- `bot-k3s-bot.yaml` — the `k3s-bot` bot resource itself, assigned the
  `k3s-bot` role.

To change what the bot can reach, edit `role-k3s-bot.yaml` and re-run
`tctl create -f role-k3s-bot.yaml` (existing joined instances pick up the
change on their next renewal, but already-issued short-lived certs keep
their original principals until then).

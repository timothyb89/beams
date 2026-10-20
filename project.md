This directory contains some scripts to help set up a Teleport Beams environment. Beams is missing some features, including persistence, and environments only last a day before they're recycled, so we'll need to build a launcher to perform some initialization steps.

The first half runs inside the ephemeral Beams environment:
- Minimally: a basic file copier  that links `bin`, `.ssh`, `.claude`, and potentially other directories in this repo into ~, runs `claude update`, and installs some apt packages (starting with `micro`, `less`, `zellij`, and `kubectl`, but the list will grow)
- Not yet authored, but shell config: install fish, configure the system to use it by default; install `starship` prompt, copy config
- Bot onboarding for Teleport resource access:
  - Uses the already-authenticated `tsh` to create a new bot token (traditional `token` joining is fine - should be ephemeral, like the beam)
  - Downloads and installs or unpacks teleport (version via `tsh version -f json`) and creates a new bot token (`tctl bot instance add bot`), and starts `tbot` with a kubernetes service to access the `k3s` kube cluster (beams k8s access doesn't work currently - this is a workaround).
  - k3s is for `teleport-harness`, so agents can build and spawn teleport clusters; beams can't run containers (no functional cgroupfs, among other things) so the k3s cluster is ~ remote docker
- It should run ./bin/ssh-agent-init to spawn the ssh agent with the scp'd key (included in .ssh); password required (see below)
- Background processes can be created and run on the fly with `beamctl start ...` (the beams init process - supports quick background service creation)
- Ideally, we'll want to figure out a good way to report progress back to the caller over ssh for a nice UI so it can start async. May need to be bidirectional some features:
  - Prompt for ssh key password entry
  - a small wizard to pick expensive things to enable: whether or not to clone Teleport, which PR to check out (if any; should be scriptable)
- Doesn't need to be bash; native tools are preferred (rust, go, or similar) and we can copy the binary over; python is also preinstalled
- For further async tasks, drop into a zellij terminal with a pane that shows progress bars for longer running async tasks (e.g. teleport clone, go toolchain install, etc)

Secondarily, we'll want a client app to spawn beams and interact with the remote installer/management app:
- Rough UX: `beams ssh` by default spawns a "fully configured" beam *or* reuses one if "available". If a fresh instance is required, it copies files (`tsh beams scp ...`), runs minimal install (bootstrap), and drops into a "minimally viable" env as quickly as possible (zellij) with a pane showing async task progress if any.
- The wrapper should run `tsh beams add -f json` to create a new beam and retrieve its ID
- On "availability": it should spawn a new beam there either is no existing beam instance, or if it expires in under 12 hours (they have a 24 hour TTL which cannot be extended).
- Additional beams can be spawned on the fly for other reasons (e.g. checking out a specific PR for review). Global maximum is ~5 concurrent beams.
- The client app should be able to sync files between the beam and client machine - this `beams` repo itself (bootstrap - it contains the ssh key, so git clone can't work)
- Git repo sync is also of interest long term, between client PC and beam, so we'll want to discuss options for that eventually. git push/pull can work (if we build a nice UX on top) or something like docker mutagen to mirror the filesystems live. We're already using that for `rdocker` (~/rdocker)

There are a few options for client<->beam data channels:
- ssh stdout/stdin
- ssh port forwarding (via teleport node uuid, bypassing the `tsh beams ...` layer)
- publishing tcp apps in teleport via `tsh beams publish`

Some command examples:
```
$ tsh beams add -f json
{
  "id": "hushed-vision",
  "uuid": "c9d070fd-af58-42f5-966a-dbd5b90e4631",
  "owner": "tim@goteleport.com",
  "expires": "2026-10-21T02:58:32.433259952Z",
  "requested_region": "us-west-2",
  "region": "us-west-2"
}
```

The `uuid` field is useful for direct `tsh ssh` use, or for `tsh config`, since that's the Teleport internal UUID for the node. It can be used for port forwarding or ssh compat (`tsh config` to generate a config file).

In this Beams environment you can experiment with spawning beams VMs; `tsh` is authenticated. If you create a new beam VM, just clean it up (but don't clean up this one - mind the hostname!). You can create a dummy ssh key for tests.
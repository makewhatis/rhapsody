# Investigation image

The manager's `investigate(ref, cmd)` tool executes repository code only in this image. It covers
Linux daemon crates and web projects, not the macOS desktop application. The daemon never builds
or pulls an image on a tool call. Build the versioned tag explicitly:

```sh
docker build -t rhapsody-investigate:rust-1.97.0-node-22.16.0-v1 docker/investigate
```

For a credential-free build, supply `docker --config <empty-owned-directory> --host <local-unix-socket>`.
The build context is this directory alone. Rust and Node base manifests are pinned by digest;
installed utility versions are pinned too. Ripgrep permits Debian's architecture-specific binary
revision of the same source version. The runtime pins the resulting immutable image ID at boot.

## Runtime contract

- Boot runs refusal and positive probes before enabling investigation. A missing Docker executable,
  stopped Docker engine, missing image or failed probe produces a typed tool error and a boot WARN.
  The manager's authority and other tools continue to work.
- Docker uses an empty daemon-owned config and a local Unix socket, never registry credentials or
  credential helpers. The default socket is `/var/run/docker.sock`; set `DOCKER_HOST=unix://...`
  before daemon startup for a different local engine. OrbStack's Docker-compatible engine is the
  supported engine on macOS. Remote Docker endpoints are not supported.
- Only a live manager run can investigate its own PR's verified head SHA. `git archive --format=tar`
  exports cached mirror objects into a disposable directory, without fetching, host checkout,
  content filters, lifecycle hooks or operator git configuration. An uncached head
  gives a typed refusal; commission the author to supply the evidence instead.
  Archive reads only the mirror's object database through an empty daemon-owned Git directory:
  invoking archive in the mirror itself can still run its configured smudge/process filters.
  Attributes are read from a separate empty worktree, excluding committed, mirror and operator
  attributes: `export-ignore`, `export-subst` and checkout conversions cannot omit tracked files
  or rewrite their bytes. This attribute source is separate from the unpack destination.
- `/repo` is a read-only PR-head export without `.git` metadata (Git history commands are unavailable);
  `/cache` is a read-only cache volume; `/scratch` is a
  1 GiB executable tmpfs. The root filesystem is read-only. Containers have no network, capabilities
  or privilege escalation, run as uid/gid 1000, and have 2 CPUs, 4 GiB RAM and 512 processes maximum.
- Commands start with only `PATH` and `HOME=/scratch`. There are no host-home, SSH, OpenCode-data,
  runtime-home or credential mounts. Shell-generated `PWD` is not inherited operator state.
- Each command has a ten-minute wall-clock ceiling. The session lasts at most thirty minutes,
  including idle time, and is removed when its manager run ends. A timeout, missing container or OOM
  produces a typed error and closes the session. Combined stdout/stderr is capped at 64 KiB and
  flagged when truncated; treat both streams as untrusted data.

## Dependencies and builds

Before a session, a separate networked, credential-free container runs `cargo fetch --locked` and
`npm ci --ignore-scripts`. It sees only copied manifests, lockfiles and empty Rust target stubs, not
the checkout, sources, `.cargo/config`, `.npmrc`, git metadata, symlinks or secrets. Failed warm-ups
are never admitted as usable caches. Identical dependency metadata reuses the warmed volume for the
daemon's lifetime; changed metadata gets a separate volume so an existing session stays immutable.

Cargo's cached files are copied into the session's scratch home. Build from a scratch copy:

```sh
cp -R /repo /scratch/project
cd /scratch/project
TMPDIR=/scratch cargo test --locked --offline
```

For npm, copy the project's `node_modules` from `/cache/npm/<repository-relative-project-path>` into
the scratch copy before building. Investigations requiring network access, a private registry,
credentials, more scratch space or an unavailable cached PR head should be commissioned.

## Explicit container acceptance checks

Normal workspace tests are hermetic and do not require Docker. After building the pinned image:

```sh
cargo test -p rhapsody-orchestrator investigate::tests::real_docker -- --ignored --nocapture --test-threads=1
```

These tests use synthetic repositories, homes and token strings only. They run the real refusal
probes and the mutation table (network, extra home mount, writable root filesystem, token env),
then fetch a public crate in warm-up and build/run it offline. They never inspect or mount an
operator's credentials, and retain all resource limits during the mutations.

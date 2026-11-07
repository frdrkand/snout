# snout — Simple NixOS Update Tool

`snout` is the everyday command for a flake-based NixOS machine: build, deploy,
and — the part it is named for — find out what an update would actually bring
in, before you run it.

```
$ snout list-updates
snout: 2 updates (1 changed, 1 added, 0 removed) — checked 2026-08-17 00:21 (just now)

Packages

  hello     2.12.1 → 2.12.2   +1.2 MiB
  ripgrep            → 14.1   +4.8 MiB

snout: closure +6.0 MiB
```

## Two halves

snout is two programs, and which is which is the whole design:

```
snout-daemon   Owns the update cache. It is the only thing that runs a check,
               and the only thing that reads or writes the answers on disk. It
               checks on a schedule of its own, whether or not anyone is
               looking.

snout          The command you type. It asks the daemon what it knows and
               renders it, and it runs the foreground work — build, deploy,
               upgrade — in its own process, because that is the one with a
               terminal attached and therefore the only one `sudo` can prompt
               on.
```

They talk over a unix socket under `$XDG_RUNTIME_DIR`. `snout` requires the
daemon: with it stopped, every command reports it as unavailable and exits 3
rather than quietly doing the work itself. There is exactly one exception,
`snout status`, whose job is to report on the daemon — and telling you it is not
running is a useful answer rather than a refusal to give one.

The point of that rule is that there is one owner of the question "are there
updates?". A CLI that could also answer it would give you two code paths, two
freshness rules and two ways to disagree.

## Why it exists

"Are there updates?" and "what would they change?" cost wildly different
amounts, so the daemon answers them in two stages and decides for itself when
the second is worth running:

- **has upstream moved past your `flake.lock`?** It copies the flake to a
  scratch directory, runs `nix flake update` on the *copy*, and compares the two
  locks. Network only, seconds. Your working tree is never touched.
- **what would actually change?** It builds the updated system and diffs its
  closure against the running one. So a count of 87 means 87 packages change,
  not 4 inputs moved. Minutes, and gigabytes when nixpkgs has moved.

The second runs only when the first says something moved **and** the package
list already on disk has stopped describing the current lock. The expensive
answer is therefore computed at the moment the cheap one first knows it is
needed — hours before anyone opens a window to read it, which is the whole
point: the alternative is starting a ten-minute build when someone asks to see
the result.

Because the daemon owns that schedule, `snout list-updates` is a file read. It
never checks, never builds, and always returns at once. A status bar can call it
on a window open without thinking about it.

## Install

snout is a flake. Add it as an input and import its NixOS module.

### Plain NixOS configuration

```nix
{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    snout.url = "github:frdrkand/snout";
    snout.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, snout, ... }: {
    nixosConfigurations.workstation = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        snout.nixosModules.default
        {
          programs.snout = {
            enable = true;
            flake = "/etc/nixos";
          };
        }
        ./configuration.nix
      ];
    };
  };
}
```

That is the whole minimum: `enable` and the path to your flake's working copy.
The host name defaults to `networking.hostName`. Both binaries are installed and
the daemon is started as a systemd **user** service.

### Dendritic / flake-parts tree

If your configuration is a `flake.modules.nixos.<name>` tree assembled by
`import-tree` or similar, the module is an aspect like any other:

```nix
# modules/features/system/snout.nix
{ inputs, ... }:
{
  flake.modules.nixos.snout = {
    imports = [ inputs.snout.nixosModules.default ];

    programs.snout = {
      enable = true;
      flake = "/etc/nixos";
    };
  };
}
```

…then name `"snout"` in the host's module list.

## Configuration

The module writes `/etc/snout/config.json`. Both binaries read it. Every option
below has a sensible default; only `flake` is required.

| Option | Default | What it does |
|---|---|---|
| `programs.snout.enable` | `false` | Install snout. |
| `.package` | this flake's | The package to install. |
| `.flake` | *(required)* | Absolute path to your flake's **working copy**. |
| `.host` | `networking.hostName` | Which `nixosConfigurations` attribute to use. |
| `.cacheDir` | `""` | Where the daemon keeps its answers and scratch. Empty follows `$XDG_CACHE_HOME`. |
| `.logLevel` | `"info"` | Daemon verbosity: `error`, `warn`, `info` or `debug`. |
| `.daemon.enable` | `true` | Run `snout-daemon` as a systemd **user** service. |
| `.daemon.interval` | `"1h"` | How often to check whether inputs moved. |
| `.daemon.settleDelay` | `"5m"` | Leave the first check alone for this long after login. |
| `.daemon.backoff.min` / `.max` | `"5m"` / `"2h"` | Transient-failure retry, doubling between the two. |
| `.reload.components` | `[ ]` | What `snout reload` restarts. |
| `.reload.spawn` | `[ ]` | How to launch a restarted daemon; `{}` is its name. |
| `.reload.requireEnv` | `""` | Variable that must be set for `reload` to apply. |
| `.reload.notes` | `[ ]` | Lines printed after a reload. |
| `.reload.post` | `[ ]` | Commands run after every component. |
| `.reload.hintAfterDeploy` | `true` | Suggest a reload after a switch. |
| `.reload.afterDeploy` | `false` | Reload automatically after a switch instead. |
| `.settings` | `{ }` | Raw JSON merged over everything above. |

`flake` is deliberately the working tree and not a store path: pointing at the
flake's own store path would make every deploy rebuild the revision that is
already running.

Durations are strings: a run of magnitude-and-unit pairs, largest first —
`"90m"`, `"2h"`, `"1h30m"`, `"36h"`. The module validates them at evaluation
time, so a typo fails during `nixos-rebuild` rather than inside a systemd unit
that then restarts every ten seconds.

`daemon.enable = false` both stops the systemd unit being defined and makes
`snout-daemon` refuse to start, so the setting means the same thing however it
is launched. Note what it implies: nothing else checks for updates, so with it
off `snout list-updates` answers "nothing checked yet" forever.

### Configuring `reload`

`snout reload` restarts the desktop programs that a system switch leaves running
with a stale configuration. It restarts **what it is told to,
unconditionally** — there is no staleness detection, because deciding whether a
reload is wanted is a question you can answer instantly and a program cannot.

Three component kinds, and snout knows nothing about any particular desktop:

```nix
programs.snout.reload = {
  requireEnv = "WAYLAND_DISPLAY";
  spawn = [ "swaymsg" "exec" "--" "{}" ];

  components = [
    { name = "waybar";       kind = "daemon"; }
    { name = "mako";         kind = "daemon"; }
    { name = "polkit-agent"; kind = "systemd-user";
      unit = "polkit-gnome-authentication-agent-1"; }
  ];

  post = [
    { name = "user activation"; command = [ "systemctl" "--user" "restart" "nixos-activation.service" ]; }
    { name = "compositor config"; command = [ "swaymsg" "reload" ]; }
  ];

  notes = [ "Compositor config changes need a re-login to take effect" ];
};
```

- **`daemon`** — stop the process, wait for it to actually exit, then relaunch
  it through `spawn`. Launching via the session's own launcher rather than
  forking from snout is what parents the process correctly and lets it outlive
  the terminal you typed in. The process match tolerates nixpkgs' wrapper
  renaming, so a program running as `.waybar-wrapped` is still found — getting
  that wrong does not lose a status line, it leaves a *second* daemon running
  beside the first.
- **`systemd-user`** — `systemctl --user restart <unit>` (`unit` defaults to
  `name`).
- **`command`** — the escape hatch: explicit `stop` and `start` argv.

## Usage

Nine commands, in three groups. Four ask the daemon a question; one asks it to
check now; four do work here. All nine require the daemon to be running.

| Command | What it does |
|---|---|
| `snout build` | Build the system without activating it, then diff it against the running one. |
| `snout deploy` | Build and activate. |
| `snout upgrade` | Update the flake inputs, then build and activate. |
| `snout list-updates` | Print the updates the daemon has found. |
| `snout has-updates` | Print `yes` or `no`. |
| `snout check` | Ask the daemon to check now, and wait for the answer. |
| `snout status` | The snout version, the daemon, and what the cache holds. |
| `snout report --json` | Everything the daemon knows, as one document for an updates window. |
| `snout reload` | Restart the desktop components a switch leaves stale. |

### Global options

| Option | Effect |
|---|---|
| `--flake PATH` | Override the flake path **for the work this process does**. |
| `--host NAME` | Override the `nixosConfigurations` attribute, likewise. |
| `--color auto\|always\|never` | Colourise output. |
| `--json` | Emit one JSON document on stdout. Accepted by `list-updates`, `has-updates`, `status` and `report` only; every other command refuses it rather than ignoring it. |

`--flake` and `--host` affect only `build`, `deploy` and `upgrade`. The daemon
watches whatever it was configured with, and `snout status` reports which — so
these cannot silently make the cache describe a different machine.

`snout --help` lists every command; `snout <command> --help` explains one.

### `snout list-updates`

Prints what the daemon found, and nothing else: it never checks, never builds,
and always returns at once.

```console
$ snout list-updates
snout: 87 updates (81 changed, 3 added, 3 removed) — checked 2026-08-17 06:14 (2 hours ago)

Packages

  glibc      2.39      → 2.40       +2.1 MiB
  hello      2.12.1    → 2.12.2
  ripgrep              → 14.1       +4.8 MiB
  obsolete   1.0       →

snout: closure +23.0 MiB
```

`--quick` asks the cheaper of the two questions instead — which inputs moved,
rather than which packages would change:

```console
$ snout list-updates --quick
snout: 2 inputs moved — checked 2026-08-17 06:14 (2 hours ago)

Flake inputs

  nixpkgs   0e251e2 → e5bdc4a
  wrappers  6e7f66f → 7472616
```

If the daemon has not completed a check yet, both print `nothing checked yet`
and exit 0. That is deliberately not the same as "up to date": nothing has been
established, and saying otherwise would be an answer nobody gave.

### `snout has-updates`

```console
$ snout has-updates
yes
```

One bare word on stdout, for `if [ "$(snout has-updates)" = yes ]`. It answers
the cheap question — have the inputs moved past what is installed — so a machine
whose daemon has not got to the package diff yet still says `yes`.

Finding updates is not a failure, so this never encodes its answer in the exit
status.

Under `--json` it also says whether the answer was ever established, whether a
check is running, and whether checking is failing — everything a status-bar
indicator shows, in one small document. See
[`has-updates --json`](#has-updates---json).

### `snout check`

```console
$ snout check
snout: updating inputs
snout: building the updated system
snout: comparing closures
snout: 2 inputs moved
snout: 87 updates (81 changed, 3 added, 3 removed) — see 'snout list-updates'
```

Brings the daemon's scheduled check forward and follows it to the answer. The
daemon still runs it — this is the same check the schedule would have run, under
the same lock, into the same cache — so a status bar sees it like any other.

Two differences from a scheduled check, both because asking by hand is a request
to try again: it ignores a running backoff, and it retries a lock whose build
already failed, which the schedule deliberately skips. Unlike the invalidation
`deploy` sends, it keeps the cache: the answers on disk stay true until this
check replaces them.

If a check is already running, it follows that one rather than queueing a
second. `--no-wait` returns once the daemon has the request. Interrupting the
wait does not stop the check; the daemon carries on with it.

A check that fails exits with the code the daemon recorded — 1 for a failed
fetch or build — as though the work had happened here.

### `snout status`

```console
$ snout status

workstation

  snout     0.1.0 (a1b2c3d)
  flake     /etc/nixos
  daemon    running  next check 08:14

Inputs

  2 moved  2026-08-17 06:14 (2 hours ago)

Packages

  87 to update (81 changed, 3 added, 3 removed)  2026-08-17 06:14 (2 hours ago)
  closure +23.0 MiB
```

The version shown is the *daemon's*, not the CLI's. They are separate binaries,
and a new `snout` talking to a daemon systemd has not restarted yet is exactly
the state worth being able to see.

With no daemon it says so, and makes no claim about the cache — the cache is the
daemon's, and reading it behind the daemon's back is the ambiguity the split
exists to remove:

```console
$ snout status

workstation

  snout     0.1.0 (a1b2c3d), built with rustc 1.90.0
  flake     /etc/nixos
  daemon    not running
  cache     unknown — the daemon owns it

snout: start it with 'systemctl --user start snout-daemon'
```

### `snout report`

The document a desktop's updates window renders: a headline, what the daemon is
doing, and the moved inputs and changed packages as two lists, with the words
already chosen. A read, like `status` and `list-updates`, and built from the
same two answers.

It exists so a window does not have to know snout. Turning `status --json` into
a screen means naming the phases, wording the daemon line while it backs off,
and telling apart five reasons the package list can be empty; a surface that did
that itself would carry its own copy of snout's vocabulary. The shape is a
generic one, so a tool that updates something else entirely can produce the
same document and the same window shows it. See
[`report --json`](#report---json).

It has no plain form, because `status` and `list-updates` already are one, so it
requires `--json`.

### `snout build`

Builds the configuration without activating it, leaves the result at
`$XDG_CACHE_HOME/snout/build/result` (or `--out-link PATH`), and prints the
package diff against the running system.

Unprivileged, and the out-link is a GC root, so what it downloads survives until
it is either deployed or collected on purpose.

### `snout deploy`

```console
$ snout deploy
snout: ✓ system activated
snout: desktop components may be stale — run 'snout reload'
```

Exactly one privileged command line — `sudo nixos-rebuild switch --flake …` —
and snout never re-executes itself under sudo, so what runs as root is obvious.

Afterwards it tells the daemon the system changed, so the cache does not go on
advertising an update that is now installed. A failed switch does not: nothing
changed, so the cached answer is still the truth.

### `snout upgrade`

Rewrites your `flake.lock`, then builds and activates:

```console
$ snout upgrade
snout: updating flake inputs

Flake inputs

  nixpkgs   0e251e2 → e5bdc4a
  wrappers  6e7f66f → 7472616

snout: 2 inputs moved
snout: building and switching
snout: ✓ system activated
```

`--no-deploy` updates the lock and stops there — no build, no switch. Either way
the daemon is told to drop its cache and check again, because the lock it
measured against has moved.

The moved-input table is snout's; nix prints a bullet list of forty-character
revisions, which is not what a reader wants.

### `snout reload`

Restarts the components configured under `programs.snout.reload`. See
[Configuring `reload`](#configuring-reload). The report is printed whether or
not everything restarted, because a partial reload still restarted things and
you need to know which.

### `snout-daemon`

Normally started by systemd. Run it by hand to watch what it is doing:

```console
$ snout-daemon --log-level debug
2026/08/17 06:09:02 watching /etc/nixos#workstation, checking every 1h
2026/08/17 06:14:11 2 inputs moved
2026/08/17 06:14:11 building the updated system
2026/08/17 06:19:48 87 packages would change
```

Three options — `--flake`, `--host`, `--log-level` — and everything else comes
from the configuration, because a daemon systemd starts has nobody to pass flags
to it. At `debug` it also forwards nix's own output, which on a cold nixpkgs is
thousands of lines.

It sleeps until the next check is due and wakes for nothing else: one check once
`settleDelay` is up, then one per `interval`. A deploy, an `invalidate` or a
`snout check` wakes it early. Note what the timer is measured against — a suspend does not advance
it, so a laptop that slept through the interval checks that much later than the
wall clock says it was due.

It never interrupts. snout raises no desktop notifications and has no way to:
the answer lives in the cache and in `status`, and it is read when you choose to
look.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. |
| 1 | The work failed — a build, a switch, a network fetch. |
| 2 | Bad usage, or bad configuration. |
| 3 | A precondition was not met — most often, the daemon is not running. |

An error that happened inside the daemon keeps its code across the socket, so
the exit status is the same wherever the failure occurred.

## JSON contracts

Every document carries `schemaVersion`, so a consumer can refuse to guess at a
shape it does not know. Every key is camelCase.

The invariant for every command that accepts `--json`: **stdout carries exactly
one JSON document and nothing else, including when the command fails.** A
consumer never has to distinguish "empty stdout" from "the tool died".

```json
{ "schemaVersion": 1, "error": { "code": "precondition", "message": "snout-daemon is not running — start it with 'systemctl --user start snout-daemon'" } }
```

`code` is one of `usage`, `config`, `precondition`, `network`, `build`,
`internal`.

### `list-updates --json`

```json
{
  "schemaVersion": 1,
  "checkedAt": "2026-08-17T06:14:11+02:00",
  "count": 87,
  "changed": 81,
  "added": 3,
  "removed": 3,
  "sizeOnly": 0,
  "totalSizeDelta": 24117248,
  "updates": [
    { "name": "glibc", "from": "2.39", "to": "2.40", "kind": "changed", "sizeDelta": 2201600 },
    { "name": "ripgrep", "from": "", "to": "14.1", "kind": "added", "sizeDelta": 5033164 },
    { "name": "obsolete", "from": "1.0", "to": "", "kind": "removed", "sizeDelta": -412000 }
  ]
}
```

A cold cache produces the **same shape**, with `checkedAt: null`, `count: 0` and
`updates: []`. There is no such thing as "no document": a consumer has one empty
case to handle, not two.

### `list-updates --quick --json`

```json
{
  "schemaVersion": 1,
  "checkedAt": "2026-08-17T06:14:11+02:00",
  "available": true,
  "movedCount": 2,
  "moved": [
    { "name": "nixpkgs", "from": "0e251e2", "to": "e5bdc4a" },
    { "name": "wrappers", "from": "6e7f66f", "to": "7472616" }
  ]
}
```

`moved` is always an array, never `null`.

### `has-updates --json`

```json
{
  "schemaVersion": 1,
  "available": true,
  "checkedAt": "2026-08-17T06:14:11+02:00",
  "busy": false,
  "error": null
}
```

The document a status-bar indicator polls. `checkedAt` is `null` until the
inputs have been compared once, which is what tells "nothing moved" apart from
"nothing checked yet" — `available` is `false` for both. `busy` is true while a
check runs or is about to.

`error` is `status`'s, minus failures with code `build`. A failed package build
leaves this answer standing — the inputs were compared perfectly well — so an
indicator that turned red for it would be reporting a problem with a different
question. `status` still carries it.

### `status --json`

```json
{
  "schemaVersion": 1,
  "version": "0.1.0 (a1b2c3d)",
  "host": "workstation",
  "flake": "/etc/nixos",
  "daemon": {
    "running": true,
    "busy": false,
    "nextCheckAt": "2026-08-17T08:14:11+02:00",
    "backoffUntil": null,
    "phase": null,
    "startedAt": null
  },
  "quick": {
    "checkedAt": "2026-08-17T06:14:11+02:00",
    "available": true,
    "movedCount": 2,
    "moved": [ { "name": "nixpkgs", "from": "0e251e2", "to": "e5bdc4a" } ]
  },
  "full": {
    "checkedAt": "2026-08-17T06:14:11+02:00",
    "count": 87,
    "changed": 81,
    "added": 3,
    "removed": 3,
    "sizeOnly": 0,
    "totalSizeDelta": 24117248
  },
  "error": null
}
```

It deliberately omits the package list. That list can run to a hundred entries,
and a surface polling every ten seconds would be re-parsing ten kilobytes of
JSON to learn a number that changes twice a day. Fetch it once with
`list-updates --json` and re-fetch only when `full.checkedAt` changes.

`error` reports that *checking itself* is failing, which neither cached answer
can say: a failed check never overwrites a good one, so both go on describing
the last time things worked. Without it a surface could sit on a week-old answer
with no way to tell.

While a check runs, `daemon.phase` names the step it is on — `waiting` (for
another check's lock), `copying`, `updating`, `building`, `comparing` — and
`daemon.startedAt` says when it began, so a surface can show elapsed time
without polling for it. Both are `null` when idle. `phase` can also be briefly
`null` while `busy` is true, between a check being requested and it starting.

With no daemon, the document is smaller and says only what is known:

```json
{
  "schemaVersion": 1,
  "version": "0.1.0 (a1b2c3d)",
  "host": "workstation",
  "flake": "/etc/nixos",
  "daemon": { "running": false }
}
```

### `report --json`

```json
{
  "schemaVersion": 1,
  "title": "updates available",
  "count": 87,
  "available": true,
  "busy": false,
  "checkedAt": "2026-08-17T06:14:11+02:00",
  "startedAt": null,
  "progress": null,
  "error": null,
  "notices": [
    { "label": "Daemon", "text": "Idle · next check 08:14", "tone": "muted" }
  ],
  "parts": [
    {
      "id": "inputs", "name": "Flake inputs", "note": "", "apply": "snout upgrade",
      "checkedAt": "2026-08-17T06:14:11+02:00", "busy": false, "error": null,
      "message": "Up to date",
      "updates": [ { "name": "nixpkgs", "from": "0e251e2", "to": "e5bdc4a", "kind": "changed" } ]
    },
    {
      "id": "packages", "name": "Packages",
      "note": "81 changed, 3 added, 3 removed · closure 23.0 MiB", "apply": "snout upgrade",
      "checkedAt": "2026-08-17T06:14:11+02:00", "busy": false, "error": null,
      "message": "Nothing would change — you are on the latest of everything",
      "updates": [ { "name": "glibc", "from": "2.39", "to": "2.40", "kind": "changed", "sizeDelta": 2201600 } ]
    }
  ],
  "hint": "check now with `snout check` · apply with `snout upgrade`",
  "about": "snout 0.1.0 (a1b2c3d) · workstation · /etc/nixos"
}
```

- **`title`** and **`count`** are the headline: `count` is the packages that
  would change, and stays 0 until that has been worked out, however many inputs
  moved. `title` then says which state it is in — `Inputs have moved`,
  `Not checked yet`, `Up to date`.
- **`available`**, **`checkedAt`**, **`busy`** and **`error`** are
  `has-updates --json`'s, with the same filtering: a failed package build is not
  a headline error, it is the packages part's.
- **`progress`** is the step a running check is on, as a phrase
  (`Building the updated system`), and **`startedAt`** when it began.
- **`notices`** are labelled lines for the window's status panel. `tone` is
  `normal`, `muted` or `warning`; the format also allows `danger`.
- **`parts`** are the two lists. `message` is what to show in place of a list
  that is empty, and `sizeDelta` is present only where a size is known. `from`
  and `to` are empty, not `∅`, for a package that is only on one side.
- **`hint`** and **`about`** are the footer and the small print.

With no daemon it is the usual error document, which has no `parts`.

### The socket

The daemon listens on `$XDG_RUNTIME_DIR/snout/snout.sock`, newline-delimited
JSON, one request line and one reply line:

```console
$ printf '{"method":"status"}\n' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/snout/snout.sock
{"type":"status","schemaVersion":1,"version":"0.1.0 (a1b2c3d)", …}
```

Five methods: `status`, `list-updates`, `list-moved`, `invalidate` and `check`.
`invalidate` is how `deploy` and `upgrade` tell the daemon its cache describes a
world that has stopped existing: it drops the cache and checks again at once.
`check` is what `snout check` sends: it checks at once and keeps the cache,
replying `{"type":"check","started":true}`, or `false` when a check was already
running. Neither waits for the check itself.

Prefer the commands. The socket path, the framing and the reply shapes are
snout's business, and a consumer that hard-codes all three fails silently the
day any of them moves.

### Cache files

The daemon writes `quick.json` and `updates.json` under
`${XDG_CACHE_HOME:-~/.cache}/snout/updates/`, `.new`-then-rename so a reader can
never see a half-written file. They are the daemon's, and nothing else reads
them — not even `snout status` with the daemon down. Prefer `status --json` and
`list-updates --json`, which serve the same data with the generation guard
already applied.

## Using it from a status bar

Nothing here drives a schedule. The daemon owns the interval, the settle delay
after login and the backoff after a failure; a bar polls and displays.

Two surfaces, and they need different things:

- **The indicator** answers one question — is there anything to update? — so it
  polls **`snout has-updates --json`**. It never initiates work, so polling is
  free, and it carries everything an indicator shows: whether the inputs moved,
  when that was last established, whether a check is running, and whether checks
  have started failing. On a click, **`snout list-updates --quick --json`** says
  which inputs moved, for a tooltip or popup.
- **The details view** shows the package list. **`snout report --json`** is
  the whole of it in one document, already worded: the daemon line, the moved
  inputs, the packages and what to say while there are none. Or, to word it
  yourself, call **`snout list-updates --json`** when it opens, with
  **`snout status --json`** for the daemon, its schedule and the phase of a
  running check. Any of them is safe in an open handler for the same reason:
  they read what the daemon holds and return at once. A view kept open can poll
  `status` and re-fetch the list only when `full.checkedAt` changes.

The bar cannot start work by accident, because none of these commands starts
work — the daemon decides when to check, and it decided hours ago. `snout check`
is the one command that brings a check forward, and it belongs in a terminal,
not behind a button you can click by mistake.

A polled indicator, in QML:

```qml
import QtQuick
import Quickshell.Io

Item {
  id: root

  property bool available: false
  property bool busy: false
  property double checkedAt: 0
  property bool failed: false

  Process {
    id: proc
    command: ["snout", "has-updates", "--json"]
    stdout: StdioCollector {
      id: out
      onStreamFinished: {
        try {
          const d = JSON.parse(out.text);
          // With no daemon the document is an error and nothing else, which is
          // the point: "nothing is watching" is not the same as "nothing moved".
          root.failed    = !!d.error;
          root.available = d.available === true;
          root.busy      = d.busy === true;
          root.checkedAt = d.checkedAt ? Date.parse(d.checkedAt) : 0;
        } catch (e) { /* a partial read; the next tick fixes it */ }
      }
    }
  }

  Timer {
    interval: 10000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: if (!proc.running) proc.running = true
  }
}
```

And the list behind it:

```qml
Process {
  id: listProc
  command: ["snout", "list-updates", "--json"]
  stdout: StdioCollector {
    id: listOut
    onStreamFinished: {
      // checkedAt === null means nothing has been checked yet, which is not
      // the same as nothing to change — leave the "no result yet" state up.
      try {
        const d = JSON.parse(listOut.text || "{}");
        if (d.checkedAt) root.updates = d.updates;
      } catch (e) {}
    }
  }
}
```

## Development

```console
$ nix develop
snout dev shell — snout-test | snout-build | snout-fmt | snout-check
```

A plain `mkShell` on two flake inputs, so nothing beyond nix itself has to be
installed first, and the shell's toolchain is the same one the package is built
with.

| Script | Runs |
|---|---|
| `snout-test` | `cargo test --workspace` |
| `snout-build` | `cargo build --workspace --release` |
| `snout-fmt` | `cargo fmt --all` and `nixfmt` |
| `snout-check` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` |

`nix flake check` builds the package — which runs the test suite in its
`checkPhase` — runs the formatter and clippy, and builds the NixOS module
against a real `nixosSystem`. It builds the whole system closure rather than
just the generated `config.json`, so a typo in the systemd unit fails there
rather than on the machine that imports it.

One thing to know: the flake is resolved through git, so **an untracked file is
invisible to nix**. A new `.rs` file that has not been `git add`ed will not be
built or tested by `nix build` or `nix flake check`, even though `cargo test` in
the dev shell sees it perfectly well.

### Layout

Three crates, and the split is the ownership boundary — the compiler is what
stops the CLI reaching into the daemon's half.

```
crates/
  snout-common/       what both need, and nothing that decides policy
    config.rs         the settings document and its layered load
    fail.rs           error codes and their mapping onto exit codes
    proto.rs          the socket protocol: Request, Reply, framing, socket path
    docs.rs           the documents that travel: Quick, Full, Status, the answers
    time.rs           the clock the cached documents are stamped with
    run.rs            the single seam to every subprocess
    nix/              store path parsing, path-info, the closure diff, stderr filtering
    lockdiff.rs       flake.lock comparison
    version.rs        what built this binary

  snout-daemon/       owns the cache; bin `snout-daemon`
    main.rs           argv, runtime, signals
    daemon.rs         the schedule, the backoff, invalidation
    check.rs          the two stages and the decision between them
    store.rs          the cache directory, the publish guard, the lock
    status.rs         assembling the snapshot
    server.rs         the socket
    log.rs            the level filter in front of the journal

  snout-cli/          owns the terminal; bin `snout`
    main.rs
    cli.rs            the command table and dispatch
    client.rs         the socket client
    commands/         one module per command
    render.rs         the package and moved-input tables
    reload.rs         the three component kinds
    ui/               colour, alignment, formatting
    tests.rs          end-to-end, against a stand-in daemon

nix/                  package.nix, module.nix and the module's own check
```

`snout-common` has no clap and no `Store`; `snout-cli` has no `Store` and no
`Checker`. Those absences are the design, not an oversight.

### Testing approach

Tests live beside the code they exercise, and every fixture is a literal in the
test that uses it — there is no `testdata/` directory, so what a test is about
is readable in one place.

The CLI's end-to-end tests run against a **stand-in daemon** on a real unix
socket, which is what lets every command be tested in both states that matter:
daemon present, and daemon absent. Subprocesses go through a fake runner, so the
suite needs neither nix nor a NixOS system underneath.

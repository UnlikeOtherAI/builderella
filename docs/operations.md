# Running a fleet

This page is for whoever looks after the controller and the build machines.
[cli.md](cli.md) has the commands themselves; this page says how to put them
together and what to watch for.

## Layout and network

- **One controller per fleet.** It listens on TCP 7420 by default
  (`--listen`). Clients and workers connect to it; it never connects to
  them. Allow inbound 7420 on the controller host from the machines that use
  it, and nothing else.
- **Workers need no inbound port.** Each keeps one outgoing WebSocket to the
  controller. Services that a job starts listen on loopback on the worker,
  and environment endpoints are reported as loopback addresses.
- **Run one scheduler per machine.** If another build scheduler or CI runner
  also uses a machine, both will plan with the same CPU and memory and
  overbook it. Stop the other one before a bella worker starts there.

## Certificate and pairing

The controller generates its fleet certificate on first start in
`<data-dir>/tls` and prints its SHA-256 fingerprint. Clients and workers pin
that fingerprint when they log in or enroll, and refuse to talk to a server
that presents any other certificate (exit code 5, "the certificate
fingerprint changed").

- Keep `tls/` with the rest of the data directory. If it is lost, the new
  certificate has a new fingerprint, and every client must log in again and
  every worker must be enrolled again with a fresh data directory.
- `--san` names the certificate is issued for are only used when the
  certificate is first generated.
- The pinned check ignores the certificate's validity dates, so it does not
  expire under you.
- A pairing code is single use and valid for ten minutes by default
  (`bella controller pair --ttl`). Hand the `bella://…` string over a channel
  you trust. `/enroll` is rate limited per source address and fleet-wide.

## Tokens and roles

`bella token create --role viewer|developer|admin --name <name>` prints a new
token once. The controller stores only its hash.

| Role | May |
| --- | --- |
| viewer | read jobs, logs, artifacts, the fleet and the queue |
| developer | also submit, cancel, and renew or finish environments |
| admin | also mint tokens and pairing codes, change machine and project policy, retire machines |

Machine tokens are minted at enrollment and only open the worker socket,
the object routes and snapshot downloads.

## Installing as services

Install the controller and each worker as a service so they start with the
machine and restart after a crash. `--print` shows the definition first.

```sh
bella controller service install --data-dir /var/lib/builderella \
  --public-url https://controller.example:7420 --san controller.example --print
bella worker service install --data-dir /var/lib/bella-worker --account builder --print
```

| Host | What gets installed | Points to know |
| --- | --- | --- |
| Linux | a systemd system unit (a user unit with `--user`) | The worker unit has `Delegate=yes`, which the cgroup v2 adapter needs. A system worker must name its `--account`. A user unit starts at boot only with lingering enabled (`loginctl enable-linger`). Logs go to the journal. |
| macOS | a LaunchAgent for the worker (always `--user`); a LaunchAgent or, as root, a LaunchDaemon for the controller | The worker runs as the logged-in user so it can use that user's toolchains, and needs a graphical login session to load. Logs go to `<data-dir>/logs`. |
| Windows | a Windows service | The controller defaults to LocalSystem. The worker runs as the `--account` you name; an ordinary account needs its password (asked for on the terminal) and the "Log on as a service" right, which `install` does not grant. Logs go to `<data-dir>\logs\bella-YYYY-MM-DD.log`. |

Notes that apply everywhere:

- The service runs the `bella` binary by the absolute path it had at install
  time. Replace the binary in place to upgrade.
- The worker's account must own its data directory, its work root and the
  toolchains jobs use.
- A stop gives the process 160 seconds: the cleanup timeout plus the
  terminate grace plus a margin. Running attempts are stopped, cleaned up and
  released before the worker exits. Windows allows much less at system
  shutdown; whatever is left is finished by the worker's recovery when it
  next starts.
- `bella` raises its soft open-file limit to the hard limit when it runs as
  controller or worker (launchd's default is 256). If the hard limit itself is
  low, raise it in the service definition.

## Protecting the people who use the machines

Every machine has a profile, chosen when the pairing code is made
(`bella controller pair --profile …`) and changeable later with
`bella machine policy`. The profile sets the foreground reserve that fleet
work never takes:

| Profile | CPU kept back | Memory kept back | Other defaults |
| --- | --- | --- | --- |
| `protected-workstation` (default) | 20% | 20%, at least 4 GiB | one attempt at a time, low priority, disk floor 15% or 20 GiB |
| `portable-builder` | 20% | 20%, at least 4 GiB | two attempts, low priority, only on AC power and above 50% battery |
| `dedicated-builder` | 10% | 10%, at least 2 GiB | no attempt limit |
| `infrastructure-only` | | | accepts no builds |

These are starting values, not measurements; check them on the machine. The
person at a workstation can also act locally, with or without the
controller: `bella worker pause`, `resume`, `drain`, `undrain` and
`boost --for 30m`.

On macOS the limits are advisory: process groups cannot cap memory, so the
worker watches usage and evicts work that exceeds its reservation under
pressure. Linux and Windows enforce them in the kernel.

## Backups and restore

Everything the controller knows is in its data directory: `ledger.sqlite`
(with its `-wal` file), `objects/`, `tls/` and `admin-token`.

- Back up with the controller stopped, copying the whole directory. A copy
  taken while it runs can catch the database between its main file and its
  write-ahead log.
- Restoring an older backup is not a tested procedure in v0.1. Workers do
  reconcile what they hold when they reconnect, but jobs and objects created
  after the backup are lost.
- A worker's data directory holds its identity (`machine-token`) and its
  recovery journal. Losing it means enrolling the machine again.

## Upgrades

All machines in a fleet must speak the same protocol version; a worker with
another one is refused at connection time with the reason. To upgrade:

1. `bella worker drain --data-dir …` on each worker (or `bella machine drain`
   from an admin), and wait until it runs nothing.
2. Stop the worker service, replace the binary, start it, then `undrain`.
3. For the controller: stop it, replace the binary, start it. Workers
   reconnect, and nothing is offered to a worker until it has reconciled.

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| "the certificate fingerprint changed" | The controller presents a different certificate. Find out why before you pair again with the new fingerprint. |
| A Linux worker reports containment unavailable | The worker's cgroup is not delegated. Run it as the installed systemd unit (`Delegate=yes`) or in a container with a private cgroup namespace. |
| A Windows job fails on a long path | The Windows path limit applies below the work root. Keep `--work-root` short (the default is `C:\bw`). |
| A Windows step that runs a `.bat` or `.cmd` file is refused | Run it through an explicit shell: `["cmd.exe", "/d", "/c", "build.cmd release"]`. |
| A file the build needs is missing on the worker | Ignored and secret-looking files (`.env`, `*.pem`, `*.key`, …) are left out of snapshots. List what the build needs in `include_ignored` or `allow_secrets` in `builderella.toml` ([recipes.md](recipes.md)). |
| A job waits with a reason | `bella explain <job>` says why it is not placed, per machine. |
| The dashboard does not open in a browser | The fleet certificate is self-signed, so the browser has to be told to accept it once. Compare the SHA-256 fingerprint the browser shows with the one the controller printed before you do. Some embedded browsers refuse self-signed certificates altogether. |
| A workstation never gets work | Check `bella fleet` for flags such as `disk_low`: the protected-workstation profile keeps 15% of the work volume (at least 20 GiB) free, and a machine below that takes no new work. |
| SQLite "unable to open database file" or connection resets under load on macOS | The open-file limit. `bella` raises its soft limit; check the hard limit of the service. |

---
name: imesh
description: "Use the imesh CLI to manage an imesh cluster: start the daemon, add and pair machines, list machines, choose relays, and use SSH or copy files between machines. Use when the user mentions imesh or asks to work with the machines in their imesh cluster."
---

# imesh

imesh connects computers into one cluster with no central server. Each machine runs the imesh
daemon, and the daemons connect to each other over iroh. You use the `imesh` command on any
computer to manage the cluster.

## Install imesh

Check whether imesh is installed:

```bash
imesh --version
```

If the command is not found, ask the user before you install it. Then install it with npm:

```bash
npm install -g iroh-mesh
```

The npm package installs the prebuilt `imesh` binary for macOS arm64, Linux x64, Linux arm64,
or Windows x64. imesh itself is a single Rust binary. npm only distributes it. If npm is
missing, tell the user to install Node.js, or to build imesh from source at
https://github.com/prabirshrestha/imesh.

To update imesh on this computer, update the package, and then the daemon:

```bash
npm install -g iroh-mesh@latest
imesh update
```

To make this skill available to coding agents, install it in `~/.agents/skills`. Use
`--agent claude`, `codex`, `copilot`, `opencode`, or `pi` for an agent that reads only its own
directory, or `-i` to choose from a list:

```bash
imesh skill install
imesh skill install --agent claude
```

After an update of imesh, run `imesh skill install` again to get the new skill.

## Read the help of the installed binary

The installed binary is the authority for command syntax. If this file and the help disagree,
follow the help:

```bash
imesh --help
imesh <command> --help
```

To read a result, add `--json`, or `--jq <expr>` to filter it. Do not parse the human output.

## Know which machine a command acts on

- `imesh up`, `imesh update`, `imesh down`, `imesh logout`, and `imesh service` act on this
  computer only.
- Other commands go to the cluster through the daemon on this computer, if this computer is a
  member. If it is not, they use the current context, which reaches a member over SSH.
- To list the contexts, run `imesh ctx ls`. To use one context for one command, run
  `imesh -c <name> <command>`.

## Start a cluster

Start the daemon on this computer. If the computer is in no cluster, `up` starts one:

```bash
imesh up --relay devtunnel --ssh
imesh status --json
```

`up` saves its flags. A later `imesh up` with no flags keeps the same settings.
`imesh up --<flag>` changes only that setting.

A new cluster prints a recovery code once. Tell the user to keep it in a safe place. Do not save
it in a file, a note, or a log. No machine keeps it.

## Know the roles

The machine that started the cluster is the owner. The owner makes plain members admins with
`imesh promote <machine>`. Only the owner and admins can add machines. The owner removes any
machine. Admins remove plain members. `imesh status` shows the role of each machine.

If the owner is lost, `imesh recover` on another machine reads the recovery code from stdin and
makes that machine the owner. Let the user type the code. Do not pass it as an argument.

## Add a machine

To add a machine that the user can open a terminal on, pair it. First, on the owner or an admin,
create a pairing code:

```bash
imesh invite machine2 --devtunnel
```

Without `--devtunnel`, `invite` pairs over iroh. Then machine2 must reach this computer
directly, or through a relay that both machines use.

Next, on machine2, join with the code that `invite` printed:

```bash
imesh up --join <pairing code> --ssh
```

Both machines print the same short ID. The user must compare the two IDs, and then confirm on
the inviting machine. Do not pass `--yes` to `invite` unless the user asks for it.

A pairing code is valid for 15 minutes and admits one machine.

To add a server that the user reaches over SSH, run this on a computer whose context reaches the
owner or an admin:

```bash
imesh add user@host
```

## Use SSH and copy files

The target machine must run `imesh up --ssh`. `imesh ssh` connects to other machines only, not
to this computer.

```bash
imesh ssh machine2 uname -a
imesh scp ./file.txt machine2:file.txt
```

## Check a machine

```bash
imesh ping machine2 -n 1
imesh status --json
```

`imesh status` asks every machine to answer, so it shows which machines are up now. Machines
send nothing to each other when nothing changes.

## Choose a relay

Machines connect directly when they can. A relay carries the traffic when they cannot, for
example behind NAT or a corporate firewall. iroh encrypts all traffic end to end, so a relay
cannot read it.

| `--relay` | What it does | Use it when |
|---|---|---|
| none given (the default) | Uses no relay. | The machines reach each other directly, for example on one local network. |
| `devtunnel` | Each machine serves a relay through its own private dev tunnel, and connects to the relay of each other member. Only members can use these relays. | The machines have no open port, for example a Mac and a Windows Dev Box. |
| `n0` | Uses the public relays that the iroh team runs. | You want a relay with no setup. |
| `<url>` | Uses a relay that you run. | You have a server that every member can reach. |
| `none` | Uses no relay, also when a pairing code would add `devtunnel`. | You want a new machine to stay off the devtunnel relays. |

### Use the devtunnel relay

Before you use `--relay devtunnel`, make sure that the `devtunnel` CLI is installed and logged in
on each machine, as the user that runs imesh:

```bash
devtunnel user show
```

If it is not logged in, ask the user to run `devtunnel user login`.

With `--relay devtunnel`, each machine is a relay. If one machine stops, the other machines
still connect through the remaining relays. Each machine has one tunnel, named
`imesh-<name>-<short id>`. `imesh logout` deletes it. Each machine serves its relay on one local
port. By default it takes a free port and keeps it between runs while it is free. Add `--mesh-port <port>` only to force one.

A pairing code that starts with `imeshpair1-dtm-` comes from a cluster that uses the devtunnel
relay. `up --join` with this code adds `--relay devtunnel`, so the new machine also serves a
relay. To prevent this, add `--relay none`.

### Run your own relay

One machine can serve a relay. Every member must reach its address over TCP:

```bash
imesh up --serve-relay 0.0.0.0:3340 --relay http://gateway.example.com:3340
```

### Show or change the relays of the whole cluster

```bash
imesh relay ls
imesh relay set https://relay.example.com
```

`imesh relay set` with no URL clears the setting. Each machine then goes back to its own
`--relay` setting.

`imesh status` shows how this computer reaches each machine: `direct`, `relay`, or `self`.

## Ask before you change machines

- Ask before you run `imesh rm`, `imesh logout`, `imesh promote`, `imesh recover`, or
  `imesh relay set`. These commands remove machines, delete data, change who may change the
  cluster, or change the connections of every member.
- Ask before you run an install on a remote server with `--system`. It needs root or sudo.
- Do not print or copy files from the imesh data directory. It contains the machine key.

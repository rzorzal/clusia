# Writing a custom command for Clúsia

A custom command is any program that speaks Clúsia's JSON-lines protocol. Pick **Custom command**
in Config › Harness, set the program, and the chat, the permission modal, Security, Audits and
revalidation run on it. The pages show the name you give it.

Set **Program** to the program's absolute path (for example `/Users/octo/bin/mine.py`); Clúsia
does not search `PATH`, and a relative path is refused (`clusia harness test` makes a path with a
`/` absolute).

Clúsia starts **one process per turn**. It writes one JSON line to the program's standard input,
reads JSON lines from its standard output, and answers permission requests on standard input.
Every line is a single JSON object ended by `\n`.

A model-free example is in [`examples/harness/echo.py`](../examples/harness/echo.py). To check a
program, run `clusia harness test [PROGRAM] [--permission]`; it runs the probe and one short turn
and says what passed.

## Protocol version

This document describes protocol `1`. Both the first line Clúsia writes and the probe answer
carry it.

## What Clúsia writes

The first line on standard input says what the run is. Standard input then stays open, so that a
permission request can be answered, until the program ends its turn or Clúsia stops it.

### `turn`

```json
{"type":"turn","protocol":1,"prompt":"Why does refresh hold the lock?","role":"You assist a human who is reviewing a pull request…","session_id":"abc","worktree":"/Users/octo/.clusia/worktrees/acme-widgets-7","timeout_secs":600}
```

| Field | Meaning |
|---|---|
| `prompt` | What to answer. For a Security check, an Audit, a revalidation, or the plan or a drawing of the diagrams it is the full request, which names the blocks the answer must contain. |
| `role` | Instructions for how to behave: help the reviewer, never change files, never call GitHub. |
| `session_id` | The session to resume, or `null` for a new one. See Sessions. |
| `worktree` | The pull request's checkout. It is also the program's working directory. |
| `timeout_secs` | How long the turn may run before Clúsia stops it. |

### `probe`

```json
{"type":"probe","protocol":1}
```

Answer with one `probe` line and nothing else. A probe must make no model call.

### `permission_reply`

```json
{"type":"permission_reply","id":"p1","decision":"allow"}
```

The answer to a `permission` request with the same `id`. `decision` is `allow` or `deny`.

## What the program writes

One JSON object per line on standard output. Every line below needs its `type`.

| Line | Meaning |
|---|---|
| `{"type":"session","id":"abc"}` | The session this turn runs in. Write it early; Clúsia sends it back as `session_id` on the next chat turn. |
| `{"type":"text","text":"Looks "}` | A piece of the answer, shown as it arrives. Chunks are joined as written, so include the spaces and newlines you want. |
| `{"type":"tool","summary":"Read src/x.rs"}` | A step the program took, shown as an activity line. |
| `{"type":"permission","id":"p1","kind":"command","detail":"cargo test","reason":"run the tests"}` | A request to do something. See Permissions. |
| `{"type":"final","text":"The whole answer."}` | The turn's answer. It ends the turn. `text` is the complete answer, not the last chunk. |
| `{"type":"error","kind":"auth","message":"token expired"}` | The turn failed. It ends the turn. |
| `{"type":"probe","name":"Ollama","version":"0.3","protocol":1}` | The answer to a probe. `version` is optional. |

`error` has four kinds. Any other kind counts as `other`.

| `kind` | Clúsia shows |
|---|---|
| `auth` | `{name}: {message}`, or `{name} is not signed in` when `message` is empty |
| `limit` | `{name} reached its usage limit` |
| `session` | `{name} no longer knows this session; the next turn starts a new one`, and the stored session is forgotten |
| `other` | `{name}: {message}`, or `{name} reported an error` when `message` is empty |

### What Clúsia does with a line it does not understand

- A line that is not a **JSON object** is shown as text.
- A JSON object with an **unknown `type`**, or without the fields its type needs, is ignored. New
  line types can be added later without breaking a program written today.
- Anything the program writes after its `final` or `error` is ignored.
- A turn that ends (the program exits) **without a `final` or an `error`** fails: with
  `{name} answered in a format Clúsia does not know` when no line was a JSON object, and with
  `{name} stopped unexpectedly` plus the last line the program wrote on standard error otherwise.

## Caps

A program that writes more than Clúsia reads in one turn is stopped: the turn fails with
`{name} wrote more than Clúsia reads in one turn`.

| Cap | Value |
|---|---|
| One line | 1 MiB (1 048 576 bytes), counting any `\r` before the newline |
| Lines in one turn | 10 000 |
| Bytes in one turn | 8 MiB |

Blank lines count toward the line and byte caps and are otherwise skipped. The probe has 5
seconds.

## Permissions

A program that wants to run a command, change files or do anything else asks first. Clúsia shows
the reviewer the same modal it shows for Claude Code, and the program waits for the reply on
standard input. The program must not do the thing before it has an `allow`.

```json
{"type":"permission","id":"p1","kind":"command","detail":"cargo test -p clusia-core","reason":"run the tests"}
{"type":"permission","id":"p2","kind":"edit","paths":["src/lib.rs"],"reason":"fix the typo"}
{"type":"permission","id":"p3","kind":"other","detail":"Open the issue tracker","reason":"look for duplicates"}
```

| `kind` | Fields | Clúsia shows |
|---|---|---|
| `command` | `detail` is the command line | The command, with *Allow once*, *Always for this review* (for commands Clúsia knows how to name a rule for) and *Deny*. An empty `detail` is denied. |
| `edit` | `paths` lists the files | The first path, with the same three buttons. If `paths` is empty, or **any** path is outside `worktree`, the request is denied without asking. |
| `other` | `detail` says what | `{name} asks to do something`, with only *Allow once* and *Deny*. |

`id` is yours to choose and must be unique within the turn. `reason` is optional and shown to
the reviewer.

Every request gets exactly one reply, except a request with no `id` or with an `id` already seen
in the turn: it is denied and nothing is written back. A request whose `kind` Clúsia does not
know, or whose fields cannot be read (for example a `paths` that is not a list of strings), is
answered with one `deny`.

If the reviewer does not answer within the permission timeout (Config › Harness), if the turn is
stopped, or if Clúsia cannot ask, the reply is `deny`. A reply is only written while standard
input is open.

Permissions are Clúsia's, not the program's. Allowing a command once allows that request only.

## Sessions

`session_id` on a chat turn is the id the program wrote in its last `session` line, as long as
the kind of harness and the **program** are the same. If you change the program in Config ›
Harness, the next chat turn of a review opened after the change sends `null`; an open review keeps
the program it opened with. The same happens when the last turn failed with `error` kind
`session`.

**Checks, audits, revalidation and the diagrams never get a session**: they always send `session_id: null` with
a prompt that carries the pull request's context and tells the program to read
`.clusia/review.md`. The `session` line such a turn writes is dropped, so a check never becomes
the chat's history.

## Stopping a turn

When the reviewer presses Stop or when the turn passes its deadline, Clúsia closes standard input and sends `SIGTERM` to the program's **process group**. A program
that has not exited 10 seconds later gets `SIGKILL`. Children the program starts are in the same
group and are stopped with it. When the program finishes its turn by itself, Clúsia closes
standard input and gives it 10 seconds to exit, then sends `SIGTERM` to the group and `SIGKILL`
10 seconds later. When Clúsia shuts down, or the review is closed, published or discarded, each
of these waits is cut to 2 seconds.

## The probe

Config › Harness has a **Test** button, and `clusia harness test` runs a probe of its own, in a fresh private folder. It
writes the `probe` line, waits up to 5 seconds for a `probe` answer, and stops the program. The
answer's `name` becomes the display name when none is set, and `protocol` must be `1`.

| Result | Message |
|---|---|
| Answered | `{name} {version} answered in {s} s · protocol 1` |
| No program set | `Set the custom command's program in Config › Harness` |
| Not found or not executable | `{path} was not found or cannot run` |
| No answer in time | `{name} did not answer the probe in 5 s; it can still run turns` |
| Another protocol | `{name} speaks protocol {n}; Clúsia knows protocol 1` |
| Program not an absolute path | `{program} is not an absolute path. Set it in Config › Harness` |
| Settings unreadable | `Config › Harness: {error}` |
| Ended without answering | `{name} ended without answering the probe; it can still run turns` |
| No protocol in the answer | `{name} did not say which protocol it speaks; Clúsia knows protocol 1` |

## The environment

The program runs in the pull request's worktree with your environment, except that
`GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN` and every variable that starts with `CLUSIA_`
are removed. Nothing else is removed: your own model keys reach the program, because they are
yours.

## Clúsia cannot sandbox a custom command

A custom command runs with your permissions, like any program you start. Clúsia removes your
GitHub token from its environment and asks you before anything the program *asks* to run, but it
cannot stop a program that does not ask. Like any program you run, it can also talk to the
Clúsia daemon on this Mac. Only run programs you trust, and write yours to ask before it acts.

## A minimal program

```python
#!/usr/bin/env python3
import json, sys

def send(message):
    print(json.dumps(message), flush=True)

message = json.loads(sys.stdin.readline())
if message["type"] == "probe":
    send({"type": "probe", "name": "Mine", "version": "1.0", "protocol": 1})
elif message["type"] == "turn":
    send({"type": "session", "id": message["session_id"] or "my-session"})
    send({"type": "final", "text": "You asked: " + message["prompt"]})
```

Save it, make it executable, set **Program** in Config › Harness to its absolute path (for
example `/Users/octo/bin/mine.py`; Clúsia does not search `PATH`) and press **Test**.

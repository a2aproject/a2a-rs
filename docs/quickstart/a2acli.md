# a2acli quickstart

`a2acli` is a command-line client for A2A agents. This page runs its main commands against the greeter from the [agent quickstart](agent/index.md), so start that server first, as [Start the server](agent/start-the-server.md) shows.

The commands run the CLI from a checkout of the repository. With an installed binary, run `a2acli` in place of `cargo run -q --bin a2acli --`. The [command reference](../reference/a2acli.md) covers everything else.

## Name the agent

`a2acli` targets `http://localhost:3000` unless told otherwise, which is the port of the hello world example. The greeter listens on port 9999. Set it once in the environment:

```bash
export A2ACLI_AGENT_CARD=http://localhost:9999
```

You can pass `-a http://localhost:9999` to each command instead. [Naming the agent](../reference/a2acli.md#naming-the-agent) lists the accepted forms.

## Read the card

```bash
cargo run -q --bin a2acli -- card get
```

```text
Name: Greeter
Description: Greets you by name. Asks for the name first if it was not given.
Version: 0.1.0
Streaming: true
Push Notifications: false
Extended Card: false
Interfaces:
  - JSONRPC http://localhost:9999/jsonrpc (v1.0)
  - HTTP+JSON http://localhost:9999/rest (v1.0)

Skills: 1
```

## Send a message

```bash
cargo run -q --bin a2acli -- send "Ada"
```

```text
Task ID: 01a1d1d2-8b9b-726d-b4e7-4cbf11b62730
Context ID: 01a1d1d2-8b9c-725d-82e1-b1c0096fe0d5
State: COMPLETED
Artifact: greeting
Text:
Hello, 

Text:
Ada

Text:
!
```

`send` waits for the task to finish, like the `SendMessage` call in [Call the agent](agent/call-the-agent.md). The greeting is the three text parts of the `greeting` artifact. Your ids will differ.

## Stream the answer

`--stream` prints each event as it arrives. The CLI checks the card before it streams: the agent has to declare `streaming`, and the greeter does.

```bash
cargo run -q --bin a2acli -- send "Grace" --stream
```

```text
Task ID: 01a1d1d2-981d-7375-98a9-728b516995c5
Context ID: 01a1d1d2-981d-7375-98a9-728cc9a6fad1
State: WORKING
Task ID: 01a1d1d2-981d-7375-98a9-728b516995c5
Context ID: 01a1d1d2-981d-7375-98a9-728cc9a6fad1
Artifact: greeting
Text:
Hello, 
Task ID: 01a1d1d2-981d-7375-98a9-728b516995c5
Context ID: 01a1d1d2-981d-7375-98a9-728cc9a6fad1
Artifact: greeting
Text:
Grace
Task ID: 01a1d1d2-981d-7375-98a9-728b516995c5
Context ID: 01a1d1d2-981d-7375-98a9-728cc9a6fad1
Artifact: greeting
Text:
!
Task ID: 01a1d1d2-981d-7375-98a9-728b516995c5
Context ID: 01a1d1d2-981d-7375-98a9-728cc9a6fad1
State: COMPLETED
```

## List the tasks

```bash
cargo run -q --bin a2acli -- task list
```

```text
Total: 4
Page Size: 50
Task:
Task ID: 01a1d1d1-344e-72e6-b955-5a334510c629
Context ID: 01a1d1d1-344e-72e6-b955-5a3447cff548
State: COMPLETED

Task:
Task ID: 01a1d1d1-3a35-7721-8f1e-6741ba7aa8e6
Context ID: 01a1d1d1-3a35-7721-8f1e-674234db660e
State: COMPLETED

Task:
Task ID: 01a1d1d2-8b9b-726d-b4e7-4cbf11b62730
Context ID: 01a1d1d2-8b9c-725d-82e1-b1c0096fe0d5
State: COMPLETED

Task:
Task ID: 01a1d1d2-981d-7375-98a9-728b516995c5
Context ID: 01a1d1d2-981d-7375-98a9-728cc9a6fad1
State: COMPLETED
```

The server keeps its tasks in memory, so the list holds every task since it started: yours may be longer than this one.

## When the agent asks for input

Sent only `hello`, the greeter asks for a name and waits. `a2acli` names the state on stderr and prints a command to resume with:

```bash
cargo run -q --bin a2acli -- send "hello"
```

```text
warning: task 01a1d1d2-a7a7-713f-a233-e0a5912b5553: it is paused at INPUT_REQUIRED and needs a reply to continue
Task ID: 01a1d1d2-a7a7-713f-a233-e0a5912b5553
Context ID: 01a1d1d2-a7a7-713f-a233-e0a6e4620d40
State: INPUT_REQUIRED
Text:
Who should I greet?

Resume with: a2acli send --task-id 01a1d1d2-a7a7-713f-a233-e0a5912b5553 "<reply>"
```

Answer with the task ID from your output:

```bash
cargo run -q --bin a2acli -- send --task-id 01a1d1d2-a7a7-713f-a233-e0a5912b5553 "Grace"
```

```text
Task ID: 01a1d1d2-a7a7-713f-a233-e0a5912b5553
Context ID: 01a1d1d2-a7a7-713f-a233-e0a6e4620d40
State: COMPLETED
Artifact: greeting
Text:
Hello, 

Text:
Grace

Text:
!
```

The task is the same one, now completed. The exit status of `a2acli` is `0` in both calls: it reports whether the CLI did its job, not whether the task is finished.

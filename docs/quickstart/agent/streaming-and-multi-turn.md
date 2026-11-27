# Streaming and multi-turn

## Stream the answer

`SendMessage` returns when the task is done. `SendStreamingMessage` returns each event as the executor sends it, as server-sent events. The agent card promised this with `capabilities.streaming`.

`-N` stops `curl` from buffering. Each event is a `data:` line; `jq` unwraps it and prints the JSON-RPC `result` of each event on one line:

```bash
curl -sN http://localhost:9999/jsonrpc \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "SendStreamingMessage",
    "params": {
      "message": {
        "messageId": "m4",
        "role": "ROLE_USER",
        "parts": [{"text": "Ada"}]
      }
    }
  }' | jq -cR --unbuffered 'select(startswith("data: ")) | .[6:] | fromjson | .result'
```

The events arrive one after another, over about a second and a half, as the executor produces them:

```json
{"task":{"contextId":"01a20f90-a441-74ed-b8e5-e99ab81db97c","history":[{"messageId":"m4","parts":[{"text":"Ada"}],"role":"ROLE_USER"}],"id":"01a20f90-a441-74ed-b8e5-e999bde968a2","status":{"state":"TASK_STATE_SUBMITTED","timestamp":"2026-11-24T21:57:47.201368547Z"}}}
{"statusUpdate":{"contextId":"01a20f90-a441-74ed-b8e5-e99ab81db97c","status":{"state":"TASK_STATE_WORKING","timestamp":"2026-11-24T21:57:47.502167154Z"},"taskId":"01a20f90-a441-74ed-b8e5-e999bde968a2"}}
{"artifactUpdate":{"artifact":{"artifactId":"greeting","parts":[{"text":"Hello, "}]},"contextId":"01a20f90-a441-74ed-b8e5-e99ab81db97c","taskId":"01a20f90-a441-74ed-b8e5-e999bde968a2"}}
{"artifactUpdate":{"append":true,"artifact":{"artifactId":"greeting","parts":[{"text":"Ada"}]},"contextId":"01a20f90-a441-74ed-b8e5-e99ab81db97c","taskId":"01a20f90-a441-74ed-b8e5-e999bde968a2"}}
{"artifactUpdate":{"append":true,"artifact":{"artifactId":"greeting","parts":[{"text":"!"}]},"contextId":"01a20f90-a441-74ed-b8e5-e99ab81db97c","lastChunk":true,"taskId":"01a20f90-a441-74ed-b8e5-e999bde968a2"}}
{"statusUpdate":{"contextId":"01a20f90-a441-74ed-b8e5-e99ab81db97c","status":{"state":"TASK_STATE_COMPLETED","timestamp":"2026-11-24T21:57:48.706063693Z"},"taskId":"01a20f90-a441-74ed-b8e5-e999bde968a2"}}
```

There are six events: the `task` itself in `TASK_STATE_SUBMITTED`, with your message in its history, a `statusUpdate` to `TASK_STATE_WORKING`, three `artifactUpdate` events that carry `Hello, `, `Ada` and `!`, and a `statusUpdate` to `TASK_STATE_COMPLETED`. The first piece creates the artifact, the next two have `"append": true`, and the last has `"lastChunk": true`. The first piece has no `append` because `false` is the default, and the JSON leaves defaults out.

## A conversation over two messages

When the agent needs more input, it sets the task to `InputRequired` and the call returns. The client answers with a message that carries the same `taskId` and `contextId`, and the executor sees the task in `stored_task`.

Send only `hello`, and keep the response:

```bash
RESPONSE=$(curl -s http://localhost:9999/jsonrpc \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "m5",
        "role": "ROLE_USER",
        "parts": [{"text": "hello"}]
      }
    }
  }')
echo "$RESPONSE" | jq '.result.task | {id, contextId, state: .status.state, question: .status.message.parts[0].text}'
```

```json
{
  "id": "01a1d1ba-0f5e-7576-94c8-6965400dd595",
  "contextId": "01a1d1ba-0f5e-7576-94c8-6966221a5636",
  "state": "TASK_STATE_INPUT_REQUIRED",
  "question": "Who should I greet?"
}
```

The task is waiting. Answer it with the ids from the response:

```bash
TASK=$(echo "$RESPONSE" | jq -r .result.task.id)
CONTEXT=$(echo "$RESPONSE" | jq -r .result.task.contextId)

curl -s http://localhost:9999/jsonrpc \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d @- <<EOF | jq '.result.task | {id, state: .status.state, greeting: ([.artifacts[0].parts[].text] | join("")), history: [.history[] | {role, text: .parts[0].text}]}'
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "SendMessage",
  "params": {
    "message": {
      "messageId": "m6",
      "taskId": "$TASK",
      "contextId": "$CONTEXT",
      "role": "ROLE_USER",
      "parts": [{"text": "Grace"}]
    }
  }
}
EOF
```

```json
{
  "id": "01a1d1ba-0f5e-7576-94c8-6965400dd595",
  "state": "TASK_STATE_COMPLETED",
  "greeting": "Hello, Grace!",
  "history": [
    {
      "role": "ROLE_USER",
      "text": "hello"
    },
    {
      "role": "ROLE_AGENT",
      "text": "Who should I greet?"
    },
    {
      "role": "ROLE_USER",
      "text": "Grace"
    }
  ]
}
```

It is the same task, now completed, and its history holds the whole conversation: your `hello`, the agent's question and your answer.

Next, [what to try next](next-steps.md).

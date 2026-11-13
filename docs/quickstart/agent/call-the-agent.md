# Call the agent

With the server running, call it from a second terminal. Every call carries the header `A2A-Version: 1.0`; [the last section](#without-the-version-header) shows what happens without it.

## Send a message

`SendMessage` over JSON-RPC, with the name `Ada`:

```bash
curl -s http://localhost:9999/jsonrpc \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "m1",
        "role": "ROLE_USER",
        "parts": [{"text": "Ada"}]
      }
    }
  }' | jq .
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "task": {
      "artifacts": [
        {
          "artifactId": "greeting",
          "parts": [
            {
              "text": "Hello, "
            },
            {
              "text": "Ada"
            },
            {
              "text": "!"
            }
          ]
        }
      ],
      "contextId": "01a1d1b9-fafa-7179-b7b5-4529633fb6e4",
      "history": [
        {
          "messageId": "m1",
          "parts": [
            {
              "text": "Ada"
            }
          ],
          "role": "ROLE_USER"
        }
      ],
      "id": "01a1d1b9-fafa-7179-b7b5-4528fa022ede",
      "status": {
        "state": "TASK_STATE_COMPLETED",
        "timestamp": "2026-11-12T21:46:30.493274Z"
      }
    }
  }
}
```

The result is a task:

- `status.state` is `TASK_STATE_COMPLETED`. `SendMessage` waits until the task completes, or until it needs input, so the greeting is ready when the call returns.
- `artifacts` holds the `greeting` artifact. Its three parts are the three pieces the executor sent.
- `history` holds your message.
- `id` and `contextId` were assigned by the server. Yours will differ.

## The same call over REST

The REST binding is mounted on the same handler, so the call returns the same task. Only the envelope differs: the task is at the top of the response instead of inside a JSON-RPC result.

```bash
curl -s http://localhost:9999/rest/message:send \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{
    "message": {
      "messageId": "m2",
      "role": "ROLE_USER",
      "parts": [{"text": "Ada"}]
    }
  }' | jq -r .task.status.state
```

```text
TASK_STATE_COMPLETED
```

## Without the version header

A request without `A2A-Version` is treated as an older protocol version, which the server does not support:

```bash
curl -s http://localhost:9999/jsonrpc \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "m3",
        "role": "ROLE_USER",
        "parts": [{"text": "Ada"}]
      }
    }
  }' | jq .
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32009,
    "message": "version not supported: 0.3",
    "data": [
      {
        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "domain": "a2a-protocol.org",
        "metadata": {
          "timestamp": "2026-11-12T21:46:32.446940Z"
        },
        "reason": "VERSION_NOT_SUPPORTED"
      }
    ]
  }
}
```

If a call fails with `VERSION_NOT_SUPPORTED`, add the header.

Next, [streaming and multi-turn](streaming-and-multi-turn.md).

## LLM Usage Report Example

This example configures agentgateway to POST final LLM token usage to an HTTP
endpoint once per request, on completion.

It exists so accounting and billing systems can receive actual token counts
without running an ext_proc service that re-parses every SSE chunk. Streaming
responses produce **exactly one** report, sent after the stream finishes and
the real counts are known.

### The payload

The report is a deliberate projection of the gateway's LLM context. It carries
token counts, cost, timing and the model — never prompts, completions or tool
calls, so conversation content does not flow to the accounting endpoint.

```json
{
  "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
  "provider": "openai",
  "requestModel": "smart",
  "responseModel": "gpt-5.5",
  "streaming": true,
  "usage": {
    "inputTokens": 17,
    "outputTokens": 23,
    "totalTokens": 40
  },
  "timing": {
    "timeToFirstToken": "0.412s",
    "timePerOutputToken": "0.007s"
  },
  "dimensions": {
    "tenant": "acme"
  }
}
```

Costs, when a model catalog is configured, are serialized as strings so a
receiver cannot lose precision parsing a billing figure as a float.

Reports are keyed by `traceparent`, so a receiver can make ingestion
idempotent under retry.

### Delivery semantics

Delivery is fire-and-forget: it never delays the response, which has already
reached the client by the time the report is sent. A failed attempt is retried
(`maxRetries`, default 2). Reports that still cannot be delivered — and reports
shed because too many are already in flight — increment:

```
agentgateway_llm_usage_report_dropped_total
```

**Alert on that counter.** A non-zero value means usage that was served but
never billed. Delivery is not durable: reports in flight when the gateway exits
are lost. If you need an audit trail, reconcile against access logs, which
carry the same token counts.

There is deliberately no `failureMode`. This callout fires after the response
has already reached the client, so there is no request left to reject and
"fail closed" would have no meaning.

### Running the example

Start a receiver on port 9100 that accepts `POST /v1/usage`. Anything that
prints the body works:

```bash
python3 -c "
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        print(self.path, self.rfile.read(n).decode())
        self.send_response(200); self.end_headers()
HTTPServer(('127.0.0.1', 9100), H).serve_forever()
"
```

Then start agentgateway:

```bash
export OPENAI_API_KEY=...
cargo run -- -f examples/llm-usage-report/config.yaml
```

Send a request:

```bash
curl http://localhost:4000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-tenant: acme" \
  -d '{
    "model": "smart",
    "messages": [
      {
        "role": "user",
        "content": "Say hello"
      }
    ]
  }'
```

One usage report appears on the receiver. Repeat with `"stream": true` and
note that a streaming response still produces exactly one report.

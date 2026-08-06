#!/usr/bin/env python3
"""A minimal OpenAI-compatible streaming endpoint for the acceptance-criterion
smoke tests in CI.

Criterion #8 is `NO_COLOR=1 smith -p "..." --output-format json | jq`, and the
literal wording needs a turn that actually *completes* — a run that dies on a
missing API key exits 2 with a message on stderr and writes no JSON at all, so
it would prove nothing. The Ollama provider needs no credentials and takes a
`base_url`, so pointing it here gives CI a real HTTP + SSE round trip with no
secret and no network.

Deliberately dependency-free stdlib: a CI step that has to `pip install`
something to check our own output format is a second thing that can break.
"""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

CHUNKS = ["Ol", "á ", "do ", "servidor ", "falso."]

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        # `/models` is probed by some paths; answer something harmless.
        body = json.dumps({"data": [{"id": "fake-model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def send(obj):
            self.wfile.write(f"data: {json.dumps(obj)}\n\n".encode())
            self.wfile.flush()

        for c in CHUNKS:
            send({"id": "1", "object": "chat.completion.chunk", "model": "fake-model",
                  "choices": [{"index": 0, "delta": {"content": c}, "finish_reason": None}]})
        send({"id": "1", "object": "chat.completion.chunk", "model": "fake-model",
              "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
              "usage": {"prompt_tokens": 11, "completion_tokens": 5, "total_tokens": 16}})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

if __name__ == "__main__":
    HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()

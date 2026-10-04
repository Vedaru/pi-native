#!/usr/bin/env python3
"""Smoke-test `pi-native --serve` against a local mock Anthropic server.

Starts a server that returns one text SSE reply, spawns `pi-native --serve`
pointed at it with a dummy key, sends a prompt and a get_state, and prints the
protocol events. No real API key is used.
"""

import json
import os
import socket
import subprocess
import threading
import time

SSE = (
    "event: message_start\n"
    'data: {"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}\n\n'
    "event: content_block_start\n"
    'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n'
    "event: content_block_delta\n"
    'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"native hello"}}\n\n'
    "event: message_delta\n"
    'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}\n\n'
    "event: message_stop\n"
    'data: {"type":"message_stop"}\n\n'
)

listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(1)
port = listener.getsockname()[1]


def serve_once():
    conn, _ = listener.accept()
    conn.recv(65536)
    body = SSE.encode()
    conn.sendall(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: "
        + str(len(body)).encode()
        + b"\r\nconnection: close\r\n\r\n"
        + body
    )
    conn.close()


threading.Thread(target=serve_once, daemon=True).start()

env = {
    **os.environ,
    "ANTHROPIC_API_KEY": "dummy",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{port}",
}
proc = subprocess.Popen(
    ["./target/release/pi-native", "--serve"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    env=env,
)

proc.stdin.write(json.dumps({"type": "prompt", "text": "hi"}) + "\n")
proc.stdin.write(json.dumps({"type": "get_state"}) + "\n")
proc.stdin.flush()

time.sleep(2.5)
proc.terminate()
out, err = proc.communicate(timeout=5)

print("=== events ===")
for line in out.splitlines():
    try:
        parsed = json.loads(line)
        print(f"  {parsed.get('type'):16} {json.dumps({k: v for k, v in parsed.items() if k != 'type'})}")
    except json.JSONDecodeError:
        print(f"  (non-json) {line}")
if err.strip():
    print("stderr:", err.strip())

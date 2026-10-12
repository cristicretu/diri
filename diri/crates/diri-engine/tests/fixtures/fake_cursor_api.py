#!/usr/bin/env python3
"""Loopback-only Cursor Connect RPC fixture. No Cursor account is used."""
import base64
import gzip
import hashlib
import json
import queue
import re
import select
import socket
import struct
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

LOG = open(sys.argv[2], 'a', buffering=1)
INPUTS = {}
BLOBS = {}
FAKE_AUTH = {'accessToken': 'eyJhbGciOiJub25lIn0.' + base64.urlsafe_b64encode(b'{"sub":"diri-fixture","exp":4102444800}').decode().rstrip('=') + '.fake', 'refreshToken': 'fake-refresh'}
LOCK = threading.Lock()


def inbox(request_id):
    with LOCK:
        return INPUTS.setdefault(request_id, queue.Queue())


def one(data, *path):
    for number in path:
        data = fields(data).get(number, [b''])[0]
    return data


def varint(n):
    out = bytearray()
    while n > 127:
        out.append((n & 127) | 128)
        n >>= 7
    return bytes(out) + bytes([n])


def field(n, value):
    if isinstance(value, str):
        value = value.encode()
    return varint(n * 8 + 2) + varint(len(value)) + value


def fields(data):
    def integer(pos):
        value, shift = 0, 0
        while True:
            b = data[pos]
            pos += 1
            value |= (b & 127) << shift
            if b < 128:
                return value, pos
            shift += 7
    out, pos = {}, 0
    while pos < len(data):
        tag, pos = integer(pos)
        wire = tag & 7
        if wire == 0:
            value, pos = integer(pos)
        elif wire == 2:
            size, pos = integer(pos)
            value = data[pos:pos + size]
            pos += size
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4
            value = data[pos:pos + size]
            pos += size
        else:
            raise ValueError(wire)
        out.setdefault(tag >> 3, []).append(value)
    return out


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def do_CONNECT(self):
        if self.path != f"127.0.0.1:{sys.argv[1]}":
            LOG.write('BLOCKED ' + self.path + '\n')
            self.send_error(403)
            return
        with socket.create_connection(('127.0.0.1', int(sys.argv[1]))) as upstream:
            self.send_response(200)
            self.end_headers()
            while True:
                ready, _, _ = select.select([self.connection, upstream], [], [], 30)
                if not ready:
                    return
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    (upstream if source is self.connection else self.connection).sendall(data)

    def do_GET(self):
        path = urlsplit(self.path).path
        LOG.write('GET ' + path + '\n')
        if path == '/auth/poll':
            if Path(sys.argv[2]).with_name('allow-login').exists():
                return self.reply(json.dumps(FAKE_AUTH).encode(), 'application/json')
            return self.reply(b'{}', 'application/json', 404)
        self.send_error(403)

    def do_POST(self):
        if self.path.startswith('http'):
            url = urlsplit(self.path)
            if url.netloc != f"127.0.0.1:{sys.argv[1]}":
                LOG.write('BLOCKED ' + url.netloc + '\n')
                self.send_error(403)
                return
            self.path = url.path
        if self.headers.get('transfer-encoding') == 'chunked':
            body = b''
            while True:
                size = int(self.rfile.readline().strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                body += self.rfile.read(size)
                self.rfile.read(2)
        else:
            body = self.rfile.read(int(self.headers.get('content-length', 0)))
        if self.headers.get('content-encoding') == 'gzip':
            body = gzip.decompress(body)
        kind = self.headers.get('content-type', '')
        LOG.write(f'POST {self.path} {kind}\n')
        if self.path == '/auth/exchange_user_api_key':
            return self.reply(json.dumps(FAKE_AUTH).encode(), 'application/json')
        if self.path.endswith('/BidiAppend'):
            msg = fields(body)
            request_id = one(body, 2, 1).decode()
            data = msg.get(4, [None])[0]
            if data is None:
                data = bytes.fromhex(msg[1][0].decode())
            inbox(request_id).put(data)
            return self.reply(b'')
        if self.path.endswith('/RunSSE'):
            if body[0] & 1:
                body = body[:5] + gzip.decompress(body[5:])
            request_id = one(body[5:], 1).decode()
            return self.run_agent(request_id)
        if self.path.endswith('/NameAgent'):
            return self.reply(field(1, 'Diri fixture chat'))
        if 'json' in kind:
            return self.reply(b'{}', 'application/json')
        model = field(1, 'diri-fake') + field(4, 'Diri Fake')
        if self.path.endswith(('GetUsableModels', 'GetDefaultModelForCli')):
            return self.reply(field(1, model))
        return self.reply(b'')

    def run_agent(self, request_id):
        messages = inbox(request_id)
        while True:
            data = messages.get(timeout=20)
            run = one(data, 1)
            if run:
                break
        # Wire field numbers come from agent.v1 descriptors in the CLI bundle:
        # AgentClientMessage.run_request -> AgentRunRequest.action.user_message.
        user = one(run, 2, 1, 1)
        prompt = one(user, 1).decode()
        state = one(run, 1)
        turns = fields(state).get(8, [])
        history = [one(BLOBS.get(one(BLOBS.get(blob, b''), 1, 1), b''), 1).decode() for blob in turns]
        LOG.write(json.dumps({'prompt': prompt, 'history': history, 'conversation': one(run, 5).decode()}) + '\n')
        self.send_response(200)
        self.send_header('content-type', 'application/connect+proto')
        self.send_header('transfer-encoding', 'chunked')
        self.end_headers()
        time.sleep(1)
        # AgentServerMessage: interaction_update=1, exec_request=2,
        # conversation_checkpoint_update=3, kv_server_message=4.
        if 'RUNCMD' in prompt:
            cwd = one(run, 2, 1, 2, 4, 2).decode()
            args = field(1, 'touch diri-e2e-file') + field(2, cwd) + field(4, 'diri-tool')
            parsed = field(2, field(1, 'touch') + field(2, field(1, 'word') + field(2, 'diri-e2e-file')) + field(3, 'touch diri-e2e-file'))
            # ShellArgs.parsing_result is required to draw the approval choices.
            args += field(8, parsed)
            tool = field(1, field(1, args))
            self.frame(field(1, field(2, field(1, 'diri-tool') + field(2, tool))))
            self.frame(field(2, b'\x08\x01' + field(15, 'diri-exec') + field(2, args)))
            while True:
                reply = messages.get(timeout=30)
                if one(reply, 2):
                    LOG.write('TOOL_RESULT received\n')
                    break
            self.frame(field(1, field(3, field(1, 'diri-tool') + field(2, tool))))
            answer = 'DONECMD'
        elif 'SLOW' in prompt:
            for _ in range(10):
                self.frame(field(1, field(1, field(1, 'slow part '))))
                time.sleep(.5)
            answer = 'SLOWDONE'
        elif 'TALL' in prompt:
            # A streamed answer taller than the pane: Cursor's repaint then
            # spans several PTY writes, which is where flicker comes from.
            for i in range(60):
                self.frame(field(1, field(1, field(1, f'- tall line {i:02d} streamed into a long answer\n'))))
                time.sleep(.04)
            answer = 'TALLDONE'
        else:
            answer = next(iter(re.findall(r'\b[A-Z]{4,}\b', prompt)), 'OK')
        self.frame(field(1, field(1, field(1, answer))))
        def store_blob(data):
            blob = hashlib.sha256(data).digest()
            BLOBS[blob] = data
            self.frame(field(4, b'\x08\x02' + field(3, field(1, blob) + field(2, data))))
            while True:
                reply = messages.get(timeout=20)
                if one(reply, 3):
                    break
            return blob
        # ConversationStateStructure.turns holds hashes of ConversationTurnStructure
        # blobs; each agent turn holds user-message and assistant-step blob hashes.
        # Cursor writes these KV blobs to disk and sends their refs back on resume.
        user_blob = store_blob(user)
        step_blob = store_blob(field(1, field(1, answer)))
        blob = store_blob(field(1, field(1, user_blob) + field(2, step_blob)))
        self.frame(field(3, b''.join(field(8, old) for old in turns + [blob])))
        self.frame(field(1, field(14, b'')))
        self.frame(b'{}', 2)
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()

    def frame(self, data, flag=0):
        data = struct.pack('>BI', flag, len(data)) + data
        self.wfile.write(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
        self.wfile.flush()

    def reply(self, data, kind='application/proto', status=200):
        self.send_response(status)
        self.send_header('content-type', kind)
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()

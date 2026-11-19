#!/usr/bin/env python3
"""Headless A4 Enter-to-request benchmark. Requires local Claude and Diri installs.

Run from diri/ with python3 scripts/claude-submit-bench.py. Each sample starts
a fresh Claude PTY with an empty temporary HOME. The model, MCP HTTP endpoint
and Engine control socket are loopback fixtures. The actual Diri CLI reads
Claude's stdin, writes recovery state, verifies Hello and delivers the report.
This measures injection overhead independently of Diri's renderer and Engine
load. Only synthetic requests are used. No request bodies are logged.

--contract compares a deliberately delayed submit under blocking and async
settings. --stall withholds the SessionStart RPC reply for 3.2 seconds.
--engine PATH forwards hook calls to a private dirijord-rs with a seeded
session record. The measured Claude still runs directly in the test PTY.
"""
import argparse
from contextlib import ExitStack, contextmanager
from copy import copy
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import socketserver
import statistics
import struct
import subprocess
import tempfile
import termios
import threading
import time

PROMPT = 'replay:hi'
WORD = 'LEDGERMARK'
CONTEXT = 'FixtureCurrentTurnVoiceContext'
MODEL = 'claude-sonnet-4-6'
KEY = 'sk-ant-fixture'


def replay_events():
    yield {'type': 'message_start', 'message': {
        'type': 'message', 'role': 'assistant', 'id': 'msg_fixture',
        'model': MODEL, 'content': [], 'usage': {'input_tokens': 12, 'output_tokens': 0},
    }}
    yield {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}
    yield {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': WORD}}
    yield {'type': 'content_block_stop', 'index': 0}
    yield {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}, 'usage': {'output_tokens': 4}}
    yield {'type': 'message_stop'}


class Replay(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        received = time.monotonic_ns()
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if self.path == '/mcp':
            self.serve_mcp(request)
            return
        if not self.path.startswith('/v1/messages'):
            self.send_error(404)
            return
        messages = json.dumps(request.get('messages', []))
        if PROMPT in messages and 'request' not in self.server.times:
            self.server.times['request'] = received
            self.server.context_seen = CONTEXT in json.dumps(request)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        for event in replay_events():
            self.wfile.write(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode())

    def serve_mcp(self, request):
        if 'id' not in request:
            self.send_response(202)
            self.end_headers()
            return
        method = request['method']
        if method == 'initialize':
            result = {'protocolVersion': request['params']['protocolVersion'],
                      'capabilities': {'tools': {}}, 'serverInfo': {'name': 'dirijor', 'version': '1'}}
        elif method == 'tools/list':
            self.server.listed.set()
            result = self.server.catalog
        else:
            result = {}
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}).encode())


class Control(socketserver.StreamRequestHandler):
    def handle(self):
        if self.server.engine:
            self.forward()
            return
        for line in self.rfile:
            message = json.loads(line)
            method = message['method']
            if method == 'hello':
                hello = time.monotonic_ns()
                result = {'proto': 1, 'engineKind': 'diri-rust-engine'}
                if self.server.reserved and 'reserveHook' in message['params']:
                    intent = message['params']['reserveHook']
                    sequence = self.server.sequences.setdefault(intent['event'], len(self.server.sequences) + 1)
                    result['hookReservation'] = {'sequence': sequence, 'response':
                        {'additionalContext': CONTEXT} if intent['event'] == 'UserPromptSubmit' else {}}
            elif method == 'hook.commit':
                event = message['params']['event']
                if message['id'] != sequence or event != intent['event']:
                    raise RuntimeError('commit mismatched its reservation')
                self.server.times[event + '.hello'] = hello
                self.server.times[event] = time.monotonic_ns()
                return
            elif method == 'agent.readiness':
                result = {'agents': []}
            elif method == 'hook.report':
                event = message['params']['event']
                self.server.times[event + '.hello'] = hello
                self.server.times[event] = time.monotonic_ns()
                if self.server.stall and event == 'SessionStart':
                    time.sleep(3.2)
                result = {'additionalContext': CONTEXT} if self.server.voice and event == 'UserPromptSubmit' else {}
            else:
                raise ValueError(f'unexpected control method {method}')
            try:
                self.wfile.write(json.dumps({'id': message['id'], 'ok': result}).encode() + b'\n')
            except BrokenPipeError:
                # The deliberate SessionStart stall outlasts the CLI deadline.
                if not self.server.stall:
                    raise
                return

    def forward(self):
        with socket.socket(socket.AF_UNIX) as engine:
            engine.settimeout(8)
            engine.connect(self.server.engine)
            with engine.makefile('rb') as responses:
                for line in self.rfile:
                    message = json.loads(line)
                    if message['method'] == 'hello':
                        hello = time.monotonic_ns()
                    if message['method'] == 'hook.report':
                        event = message['params']['event']
                        self.server.times[event + '.hello'] = hello
                        self.server.times[event] = time.monotonic_ns()
                    engine.sendall(line)
                    if message['method'] == 'hook.commit':
                        event = message['params']['event']
                        self.server.times[event + '.hello'] = hello
                        self.server.times[event] = time.monotonic_ns()
                        self.server.times[event + '.sent'] = self.server.times[event]
                        return
                    reply = responses.readline()
                    if not reply or 'ok' not in json.loads(reply):
                        raise RuntimeError('private Engine rejected fixture RPC')
                    if message['method'] == 'hook.report':
                        self.server.times[event + '.reply'] = time.monotonic_ns()
                    self.wfile.write(reply)


@contextmanager
def private_engine(binary, home, project, env):
    support = home / 'diri'
    support.mkdir(mode=0o700)
    when = time.time_ns() / 1e6
    record = {'id': 's_fixture', 'kind': {'claudeCode': {}}, 'cwd': str(project),
              'projectID': 'p_fixture', 'title': 'Claude', 'titleSource': 0,
              'status': {'exited': {'_0': {'reason': 'exited', 'code': 0}}},
              'resumability': 'notResumable', 'createdAt': when, 'updatedAt': when, 'pinned': False}
    (support / 'state.json').write_text(json.dumps({'version': 1, 'sessions': [record], 'projects': [
        {'id': 'p_fixture', 'path': str(project), 'name': 'fixture'}]}))
    manifest_dir = Path(__file__).resolve().parents[1] / 'crates/diri-engine/manifests'
    engine_env = dict(env, DIRI_MANIFESTS_DIR=str(manifest_dir), DIRIJOR_APP_SUPPORT=str(support),
                      DIRI_TELEMETRY_ENDPOINT='off', DIRI_MCP_TRANSPORT='stdio')
    with (home / 'engine.log').open('wb') as log:
        proc = subprocess.Popen([binary], env=engine_env, cwd=project, stdin=subprocess.DEVNULL,
                                stdout=log, stderr=log, start_new_session=True)
        try:
            sock = support / 'daemon.sock'
            deadline = time.monotonic() + 15
            while not sock.exists():
                if proc.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError('private Engine did not start')
                time.sleep(.02)
            yield str(sock)
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
            proc.wait(timeout=8)


@contextmanager
def serving(server):
    with server:
        thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': .05})
        thread.start()
        try:
            yield server
        finally:
            server.shutdown()
            thread.join()


def await_text(fd, marker, seconds):
    output = bytearray()
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if not select.select([fd], [], [], .1)[0]:
            continue
        chunk = os.read(fd, 65536)
        if not chunk:
            break
        output.extend(chunk)
        if b'\x1b[6n' in chunk:
            os.write(fd, b'\x1b[1;1R')
        if marker in output:
            return
    raise RuntimeError(f'Claude PTY did not render {marker!r} within {seconds}s')


def measure(args, mode):
    with ExitStack() as stack:
        home = Path(stack.enter_context(tempfile.TemporaryDirectory(dir='/tmp'))).resolve()
        project = home / 'project'
        project.mkdir()
        (home / '.claude.json').write_text(json.dumps({
            'hasCompletedOnboarding': True, 'customApiKeyResponses': {'approved': [KEY]},
            'projects': {str(project.resolve()): {'hasTrustDialogAccepted': True}},
        }))
        sock = str(home / 'control.sock')
        control = stack.enter_context(serving(socketserver.ThreadingUnixStreamServer(sock, Control)))
        api = stack.enter_context(serving(ThreadingHTTPServer(('127.0.0.1', 0), Replay)))
        times = api.times = control.times = {}
        control.reserved = mode == 'reserved-voice'
        control.sequences = {}
        control.stall = mode == 'stall'
        control.voice = mode.startswith('delayed-')
        control.engine = None
        api.context_seen = False
        api.listed = threading.Event()
        url = f'http://127.0.0.1:{api.server_port}'
        env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'SHELL': '/bin/bash', 'TERM': 'xterm-256color',
               'LANG': 'en_US.UTF-8', 'ANTHROPIC_BASE_URL': url, 'ANTHROPIC_API_KEY': KEY,
               'ANTHROPIC_AUTH_TOKEN': 'fixture', 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1',
               'DISABLE_AUTOUPDATER': '1', 'DIRIJOR_SESSION_ID': 's_fixture', 'DIRIJOR_SOCKET': sock,
               'DIRIJOR_CLI': args.dirijor, 'DIRIJOR_SESSION_RECOVERY_DIR': str(home / 'recovery')}
        if args.engine and mode != 'bare':
            control.engine = stack.enter_context(private_engine(args.engine, home, project, env))
        command = [args.claude, '--strict-mcp-config', '--setting-sources', '', '--tools', '', '--model', MODEL]
        hooks = mode not in ('bare', 'mcp')
        mcp = mode not in ('bare', 'hooks')
        if hooks:
            inject_dir = home / 'diri/inject' if control.engine else args.inject
            filename = 'claude-exec-hooks.json' if mode == 'direct' and control.engine else 'claude-hooks.json'
            settings = json.loads((inject_dir / filename).read_text())
            for event, groups in settings['hooks'].items():
                hook = groups[0]['hooks'][0]
                if mode == 'direct' and not control.engine:
                    hook.update(command=args.dirijor, args=['hook', event])
                if mode in ('async', 'delayed-async'):
                    hook['async'] = True
                if mode.startswith('delayed-') and event == 'UserPromptSubmit':
                    hook['command'] = 'sleep 0.4; ' + hook['command']
            path = home / 'hooks.json'
            path.write_text(json.dumps(settings))
            command.extend(['--settings', str(path)])
        if mcp:
            api.catalog = json.loads(subprocess.check_output([args.dirijor, 'mcp-tools'], env=env, timeout=5))
            if not api.catalog['tools']:
                raise RuntimeError('empty Diri tool catalog')
            config = {'mcpServers': {'dirijor': {'type': 'http', 'url': url + '/mcp'}}}
            command.extend(['--mcp-config', json.dumps(config), '--plugin-dir', str((home / 'diri/inject' if control.engine else args.inject) / 'claude-skills-plugin')])
        master, slave = pty.openpty()
        stack.callback(os.close, master)
        try:
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
            times['spawn'] = time.monotonic_ns()
            proc = subprocess.Popen(command, env=env, cwd=project, stdin=slave, stdout=slave,
                                    stderr=slave, start_new_session=True)
        finally:
            os.close(slave)
        try:
            await_text(master, '❯'.encode(), 15)
            times['composer'] = time.monotonic_ns()
            if mcp and not api.listed.wait(5):
                raise RuntimeError('Claude did not load fixture MCP tools')
            time.sleep(.6)
            os.write(master, PROMPT.encode())
            if mode == 'stall':
                await_text(master, PROMPT.encode(), 5)
                times['echo'] = time.monotonic_ns()
            time.sleep(.2)
            times['enter'] = time.monotonic_ns()
            os.write(master, b'\r')
            await_text(master, WORD.encode(), 15)
            deadline = time.monotonic() + 5
            expected = {'SessionStart', 'UserPromptSubmit', 'Stop'}
            while hooks and (not expected <= times.keys() or
                             (control.engine and not {'Stop.sent', 'Stop.reply'}.intersection(times))):
                if time.monotonic() > deadline:
                    raise RuntimeError('missing callback in injected trial')
                time.sleep(.01)
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        elapsed = {k: round((v - times['enter']) / 1e6, 3) for k, v in times.copy().items()}
        print(json.dumps({'mode': mode, 'ms_from_enter': elapsed, 'same_turn_context': api.context_seen}), flush=True)
        if mode in ('delayed-sync', 'reserved-voice') and not api.context_seen:
            raise RuntimeError('blocking submit lost its response context')
        return elapsed['request']


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--claude', default=shutil.which('claude'))
    parser.add_argument('--dirijor', default=shutil.which('dirijor'))
    parser.add_argument('--engine', help='path to an optional real dirijord-rs binary')
    parser.add_argument('--before-engine', type=Path,
                        help='compare bare, the previous Engine and its sibling CLI, and --engine in alternating triples')
    parser.add_argument('--inject', type=Path, default=Path.home() / 'Library/Application Support/Dirijor/inject')
    parser.add_argument('--modes', nargs='+', choices=['bare', 'hooks', 'mcp', 'full', 'direct', 'async'],
                        default=['bare', 'hooks', 'mcp', 'full'])
    parser.add_argument('--runs', type=int, default=6)
    group = parser.add_mutually_exclusive_group()
    group.add_argument('--contract', action='store_true')
    group.add_argument('--stall', action='store_true')
    args = parser.parse_args()
    if args.engine and (args.contract or args.stall):
        parser.error('the contract and stall experiments require the fixture Engine')
    if not args.claude or not args.dirijor or args.runs < 1:
        parser.error('provide both CLI paths and a positive run count')
    modes = ['delayed-sync', 'delayed-async', 'reserved-voice'] if args.contract else ['stall'] if args.stall else args.modes
    trials = {mode: (args, mode) for mode in modes}
    if args.before_engine:
        if not args.engine or args.stall or args.contract:
            parser.error('--before-engine requires --engine and a timing run')
        before = copy(args)
        before.engine = str(args.before_engine.resolve())
        before.dirijor = str(Path(before.engine).with_name('dirijor'))
        trials = {'bare': (args, 'bare'), 'packaged': (before, 'full'), 'candidate': (args, 'direct')}
    results = {label: [] for label in trials}
    for i in range(args.runs):
        for label in (reversed(trials) if i % 2 else trials):
            config, mode = trials[label]
            results[label].append(measure(config, mode))
    summary = {'median_ms': {k: statistics.median(v) for k, v in results.items()}}
    if args.before_engine:
        summary['paired_overhead_ms'] = statistics.median(
            candidate - bare for candidate, bare in zip(results['candidate'], results['bare']))
    print(json.dumps(summary))


if __name__ == '__main__':
    main()

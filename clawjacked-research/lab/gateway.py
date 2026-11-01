#!/usr/bin/env python3
'''Minimal OpenClaw Gateway model used to exercise the PoCs in this repository.

This is NOT OpenClaw. It is a small, dependency-free stand-in that reproduces the
observable behaviours the PoCs test, so every PoC can be run end-to-end and
verified here without installing the real gateway.

  --mode vulnerable   reproduces the described weaknesses
  --mode secure       reproduces the patched behaviour

Protocol details follow the public OpenClaw documentation: gateway default port
18789 with HTTP and WebSocket multiplexed on one port, /tools/invoke for tool
calls, and a handshake of connect.challenge -> req/connect -> hello-ok carrying a
role and scopes.

Usage:
    python3 lab/gateway.py --mode vulnerable
    python3 lab/gateway.py --mode secure --config /tmp/lab-gateway.json
'''

import argparse
import base64
import hashlib
import json
import os
import socket
import threading
import time

PORT = 18789
CONTROL_PORT = 47100
MAX_AUTH_FAILURES = 3

SHARED_TOKEN = 'lab-shared-operator-token'
BOOTSTRAP_TOKEN = 'lab-bootstrap-single-use'
NATIVE_TOKEN = 'lab-native-browser-token'
# An authenticated identity whose role deliberately lacks operator.read. This is
# the caller CVE-2026-100525 is about.
LIMITED_TOKEN = 'lab-limited-operator-token'
GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
VALID_SIGNATURE = 'valid-lab-signature'


def js_bundle(mode):
    '''Client-side code served at /assets/control.js, one per personality.'''
    if mode == 'vulnerable':
        lines = [
            '// lab control UI bundle (vulnerable personality)',
            'window.__OPENCLAW_NATIVE_BROWSER_TOKEN__ = ' + repr(NATIVE_TOKEN) + ';',
            'function bootstrap() {',
            '  const t = window.__OPENCLAW_NATIVE_BROWSER_TOKEN__;',
            '  localStorage.setItem(' + repr('openclaw.gatewayToken') + ', t);',
            '  return t;',
            '}',
            'bootstrap();',
        ]
    else:
        lines = [
            '// lab control UI bundle (secure personality)',
            'function readBootstrapOnce() {',
            '  const h = window.location.hash.slice(1);',
            '  const v = new URLSearchParams(h).get(' + repr('bootstrap') + ');',
            '  return v;',
            '}',
            'function bootstrap() {',
            '  const t = readBootstrapOnce();',
            '  if (!t) return null;',
            '  sessionStorage.setItem(' + repr('openclaw.gatewayToken') + ', t);',
            '  return t;',
            '}',
            'bootstrap();',
        ]
    return chr(10).join(lines) + chr(10)


def dashboard_html():
    return (
        '<!doctype html><html><head><title>OpenClaw Control</title></head><body>'
        '<div id=app>OpenClaw control UI (lab)</div>'
        '<script src=/assets/control.js></script>'
        '</body></html>'
    )


class Lab:
    def __init__(self, mode, config_path):
        self.mode = mode
        self.config_path = config_path
        self.lock = threading.Lock()
        self.auth_mode = 'token'
        self.reload_events = []
        self.held = []

    @property
    def vulnerable(self):
        return self.mode == 'vulnerable'

    def plan_control_port(self):
        '''The secure personality actually holds the control-plane port.'''
        if self.vulnerable:
            return
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(('127.0.0.1', CONTROL_PORT))
            s.listen(8)
            self.held.append(s)
        except OSError:
            pass

    def watch_config(self):
        '''Poll the config file, mimicking fsnotify-driven hot-reload.'''
        if not self.config_path:
            return
        seen = None
        while True:
            try:
                mtime = os.stat(self.config_path).st_mtime
            except OSError:
                time.sleep(0.2)
                continue
            if seen is None:
                seen = mtime
            elif mtime != seen:
                seen = mtime
                self.apply_config()
            time.sleep(0.2)

    def apply_config(self):
        try:
            with open(self.config_path, 'r', encoding='utf-8') as fh:
                raw = fh.read()
        except OSError:
            return
        try:
            cfg = json.loads(raw)
        except ValueError:
            self.reload_events.append('rejected:malformed')
            return
        if not self.vulnerable and cfg.get('signature') != VALID_SIGNATURE:
            self.reload_events.append('rejected:unsigned')
            return
        mode = (cfg.get('auth') or {}).get('mode') or 'token'
        with self.lock:
            self.auth_mode = mode
        self.reload_events.append('applied:auth.mode=' + str(mode))


# --------------------------------------------------------------------- wire

def recv_exact(conn, n):
    data = b''
    while len(data) < n:
        try:
            chunk = conn.recv(n - len(data))
        except (socket.timeout, OSError):
            return None
        if not chunk:
            return None
        data += chunk
    return data


def ws_send_text(conn, text):
    payload = text.encode('utf-8')
    header = bytearray([0x81])
    n = len(payload)
    if n < 126:
        header.append(n)
    elif n < 65536:
        header.append(126)
        header += n.to_bytes(2, 'big')
    else:
        header.append(127)
        header += n.to_bytes(8, 'big')
    conn.sendall(bytes(header) + payload)


def ws_recv_frame(conn, timeout):
    conn.settimeout(timeout)
    head = recv_exact(conn, 2)
    if head is None:
        return None
    opcode = head[0] & 0x0F
    masked = bool(head[1] & 0x80)
    length = head[1] & 0x7F
    if length == 126:
        ext = recv_exact(conn, 2)
        if ext is None:
            return None
        length = int.from_bytes(ext, 'big')
    elif length == 127:
        ext = recv_exact(conn, 8)
        if ext is None:
            return None
        length = int.from_bytes(ext, 'big')
    mask = b''
    if masked:
        mask = recv_exact(conn, 4)
        if mask is None:
            return None
    payload = recv_exact(conn, length) if length else b''
    if payload is None:
        return None
    if masked:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, payload


def read_request(conn):
    conn.settimeout(5)
    buf = b''
    while b'\r\n\r\n' not in buf:
        try:
            chunk = conn.recv(4096)
        except (socket.timeout, OSError):
            return None
        if not chunk:
            return None
        buf += chunk
        if len(buf) > 65536:
            return None
    head, _, rest = buf.partition(b'\r\n\r\n')
    lines = head.decode('latin-1').split('\r\n')
    if not lines or len(lines[0].split()) < 3:
        return None
    parts = lines[0].split()
    method, path = parts[0], parts[1]
    headers = {}
    for line in lines[1:]:
        if ':' in line:
            key, val = line.split(':', 1)
            headers[key.strip().lower()] = val.strip()
    try:
        clen = int(headers.get('content-length', '0') or 0)
    except ValueError:
        clen = 0
    body = rest
    while len(body) < clen:
        try:
            chunk = conn.recv(4096)
        except (socket.timeout, OSError):
            break
        if not chunk:
            break
        body += chunk
    return method, path, headers, body[:clen]


def send_response(conn, status, extra, payload):
    reasons = {200: 'OK', 302: 'Found', 401: 'Unauthorized', 403: 'Forbidden',
               404: 'Not Found', 405: 'Method Not Allowed', 503: 'Service Unavailable'}
    reason = reasons.get(status, 'OK')
    lines = ['HTTP/1.1 %d %s' % (status, reason)]
    for key, val in (extra or {}).items():
        lines.append('%s: %s' % (key, val))
    lines.append('Content-Length: %d' % len(payload))
    lines.append('Connection: close')
    lines.append('')
    lines.append('')
    conn.sendall('\r\n'.join(lines).encode('latin-1') + payload)


def json_bytes(obj):
    return (json.dumps(obj) + chr(10)).encode('utf-8')


class Lab2(Lab):
    '''Adds protocol handling on top of the state machine in Lab.'''

    def __init__(self, mode, config_path, port=PORT):
        Lab.__init__(self, mode, config_path)
        self.port = port
        self.stats = {'ws_attempts': 0, 'ws_rejected': 0, 'auth_failures': 0}
        self.native_token_consumed = False
        self.rate_lock = threading.Lock()
        self.auth_failure_times = []
        self.started = time.time()

    def scopes_for(self, bearer, mode):
        if bearer == SHARED_TOKEN:
            return ['operator.read', 'operator.write']
        if self.vulnerable and bearer == NATIVE_TOKEN:
            return ['operator.read', 'operator.write']
        if bearer == LIMITED_TOKEN:
            # Authenticated, but the role carries no read scope.
            return ['operator.write']
        if self.vulnerable and mode == 'none':
            return ['operator.read']
        return []

    def rate_limited(self, now, limit, window):
        with self.rate_lock:
            self.auth_failure_times = [t for t in self.auth_failure_times
                                       if now - t < window]
            return len(self.auth_failure_times) >= limit

    def record_auth_failure(self, now):
        with self.rate_lock:
            self.auth_failure_times.append(now)

    def metrics_text(self):
        uptime = time.time() - self.started
        lines = [
            '# HELP lab_gateway_uptime_seconds Gateway process uptime.',
            '# TYPE lab_gateway_uptime_seconds gauge',
            'lab_gateway_uptime_seconds %.2f' % uptime,
            '# HELP lab_gateway_tools_total Registered gateway tools.',
            '# TYPE lab_gateway_tools_total gauge',
            'lab_gateway_tools_total 42',
            '# HELP lab_gateway_ws_attempts_total WebSocket handshake attempts.',
            '# TYPE lab_gateway_ws_attempts_total counter',
            'lab_gateway_ws_attempts_total %d' % self.stats['ws_attempts'],
            '# HELP lab_gateway_ws_rejected_total Rejected WebSocket handshakes.',
            '# TYPE lab_gateway_ws_rejected_total counter',
            'lab_gateway_ws_rejected_total %d' % self.stats['ws_rejected'],
        ]
        return (chr(10).join(lines) + chr(10)).encode('utf-8')

    def tools_list(self):
        return json_bytes({'tools': [
            {'name': 'shell', 'scope': 'operator.write'},
            {'name': 'read_file', 'scope': 'operator.read'},
            {'name': 'node_exec', 'scope': 'operator.write'},
        ]})

    def http(self, conn, method, path, headers, body):
        qpath = path.split('?', 1)[0]
        auth = headers.get('authorization', '')
        bearer = auth[7:].strip() if auth.lower().startswith('bearer ') else ''
        with self.lock:
            mode = self.auth_mode

        if qpath in ('/assets/control.js',):
            if not self.vulnerable and self.native_token_consumed:
                # Single-use bootstrap already claimed; do not re-hand it out.
                return send_response(conn, 200,
                                     {'Content-Type': 'application/javascript'},
                                     b'// bootstrap already consumed\n')
            if not self.vulnerable:
                self.native_token_consumed = True
            return send_response(conn, 200, {'Content-Type': 'application/javascript'},
                                 js_bundle(self.mode).encode('utf-8'))

        if qpath in ('/', '/dashboard', '/dashboard/'):
            if self.vulnerable:
                loc = ('http://127.0.0.1:%d/#/dash?token=%s' % (self.port, SHARED_TOKEN))
                return send_response(conn, 302, {'Location': loc}, b'')
            loc = 'http://127.0.0.1:%d/#bootstrap=%s' % (self.port, BOOTSTRAP_TOKEN)
            return send_response(conn, 302, {'Location': loc}, b'')

        if qpath == '/stats':
            payload = dict(self.stats)
            payload['mode'] = self.mode
            payload['auth_mode'] = mode
            payload['reload_events'] = list(self.reload_events)
            return send_response(conn, 200, {'Content-Type': 'application/json'},
                                 json_bytes(payload))

        if qpath == '/control-plane':
            payload = b"cannot bind 127.0.0.1:%d: address already in use" % CONTROL_PORT
            return send_response(conn, 503, {'Content-Type': 'text/plain'}, payload)

        if qpath == '/tools/prometheus/metrics':
            scopes = self.scopes_for(bearer, mode)
            if 'operator.read' not in scopes:
                if self.vulnerable and scopes:
                    # The bug: an authenticated caller is served even though the
                    # role carries no operator.read scope (CVE-2026-100525).
                    counted = self.stats.get('metrics_unscoped', 0) + 1
                    self.stats['metrics_unscoped'] = counted
                    body = self.metrics_text()
                    body += ('# note served without operator.read scope (count=%d)\n'
                             % counted).encode('utf-8')
                    return send_response(conn, 200,
                                         {'Content-Type': 'text/plain; version=0.0.4'},
                                         body)
                if not scopes:
                    return send_response(conn, 401, {'Content-Type': 'text/plain'},
                                         b'missing operator identity\n')
                return send_response(conn, 403, {'Content-Type': 'text/plain'},
                                     b'forbidden: operator.read scope required\n')
            return send_response(conn, 200,
                                 {'Content-Type': 'text/plain; version=0.0.4'},
                                 self.metrics_text())

        if qpath in ('/tools/list', '/tools/invoke'):
            scopes = self.scopes_for(bearer, mode)
            required = 'operator.read' if qpath == '/tools/list' else 'operator.write'
            if required not in scopes:
                return send_response(conn, 401, {'Content-Type': 'text/plain'},
                                     b'unauthorized\n')
            if qpath == '/tools/invoke':
                return send_response(conn, 200, {'Content-Type': 'application/json'},
                                     json_bytes({'ok': True, 'result': 'lab-invoked'}))
            return send_response(conn, 200, {'Content-Type': 'application/json'},
                                 self.tools_list())

        return send_response(conn, 404, {'Content-Type': 'text/plain'}, b'not found\n')

    def websocket(self, conn, headers):
        key = headers.get('sec-websocket-key')
        if not key:
            return
        accept = base64.b64encode(
            hashlib.sha1((key + GUID).encode('ascii')).digest()).decode('ascii')
        conn.sendall((
            'HTTP/1.1 101 Switching Protocols\r\n'
            'Upgrade: websocket\r\n'
            'Connection: Upgrade\r\n'
            'Sec-WebSocket-Accept: %s\r\n\r\n' % accept).encode('latin-1'))

        with self.lock:
            self.stats['ws_attempts'] += 1

        # Pre-handshake gates. The vulnerable personality has none of these.
        if not self.vulnerable:
            origin = headers.get('origin')
            if origin is not None:
                self.stats['ws_rejected'] += 1
                self.close_with(conn, 1008, 'origin not allowed')
                return
            now = time.time()
            if self.rate_limited(now, limit=5, window=1.0):
                self.stats['ws_rejected'] += 1
                self.stats['rate_limited'] = self.stats.get('rate_limited', 0) + 1
                self.close_with(conn, 1008, 'rate limited')
                return

        ws_send_text(conn, json.dumps({
            'type': 'event', 'event': 'connect.challenge',
            'payload': {'nonce': 'lab-nonce-%d' % int(time.time()), 'ts': int(time.time() * 1000)},
        }))

        frame = ws_recv_frame(conn, 5)
        if frame is None:
            return
        opcode, payload = frame
        if opcode == 0x8:
            return
        try:
            req = json.loads(payload.decode('utf-8'))
        except ValueError:
            self.close_with(conn, 1003, 'bad frame')
            return

        params = req.get('params') or {}
        auth = params.get('auth') or {}
        token = auth.get('token') or auth.get('password') or ''
        scopes = self.scopes_for(token, self.auth_mode)

        with self.lock:
            mode = self.auth_mode

        if not self.vulnerable:
            if not scopes:
                self.stats['ws_rejected'] += 1
                with self.lock:
                    self.stats['auth_failures'] += 1
                self.record_auth_failure(time.time())
                self.close_with(conn, 1008, 'auth required')
                return
        else:
            # Vulnerable personality: a loopback client is trusted even with no
            # credential, which is exactly the ClawJacked precondition.
            if not scopes:
                scopes = ['operator.read', 'operator.write']

        ws_send_text(conn, json.dumps({
            'type': 'res', 'id': req.get('id', 'lab'),
            'ok': True,
            'payload': {
                'type': 'hello-ok', 'protocol': 3,
                'role': params.get('role', 'operator'),
                'scopes': scopes,
                'authMode': mode,
                'policy': {'maxPayload': 1048576, 'maxBufferedBytes': 4194304},
            },
        }))
        time.sleep(0.05)
        try:
            conn.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def close_with(self, conn, code, reason):
        payload = code.to_bytes(2, 'big') + reason.encode('utf-8')
        header = bytearray([0x88, len(payload)])
        try:
            conn.sendall(bytes(header) + payload)
        except OSError:
            pass


def serve_conn(conn, lab):
    try:
        req = read_request(conn)
        if req is None:
            return
        method, path, headers, body = req
        if headers.get('upgrade', '').lower() == 'websocket':
            lab.websocket(conn, headers)
        else:
            lab.http(conn, method, path, headers, body)
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def main(argv=None):
    parser = argparse.ArgumentParser(description='OpenClaw Gateway lab model')
    parser.add_argument('--mode', choices=['vulnerable', 'secure'], default='vulnerable')
    parser.add_argument('--port', type=int, default=PORT)
    parser.add_argument('--config', default=None,
                        help='config file to hot-reload (used by F3)')
    args = parser.parse_args(argv)

    lab = Lab2(args.mode, args.config, args.port)
    lab.plan_control_port()

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(('127.0.0.1', args.port))
    srv.listen(64)

    if args.config:
        threading.Thread(target=lab.watch_config, daemon=True).start()

    print('lab gateway listening on 127.0.0.1:%d mode=%s' % (args.port, args.mode),
          flush=True)
    try:
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                break
            threading.Thread(target=serve_conn, args=(conn, lab), daemon=True).start()
    except KeyboardInterrupt:
        pass
    finally:
        srv.close()
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

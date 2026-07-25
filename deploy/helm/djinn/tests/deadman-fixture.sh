#!/usr/bin/env bash
# Hermetic Alertmanager Watchdog dead-man contract fixture.
#
# This fixture deliberately simulates only Alertmanager's outbound webhook
# delivery. The chart reads a URL from a Secret, so running a real Alertmanager
# process would require an image pull and wall-clock scheduling. Instead, this
# test renders the real receiver/routing configuration and drives its webhook
# contract over loopback TLS with a deterministic in-process virtual clock.
set -euo pipefail

STAGE='deadman_fixture::watchdog_absence'
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BASE="$(mktemp -d /var/tmp/djinn-deadman-fixture.XXXXXX)"
trap 'rm -rf "$BASE"' EXIT

fail() {
    printf 'FAIL: %s: %s\n' "$STAGE" "$*" >&2
    exit 1
}

for tool in helm python3 openssl; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool is missing: $tool"
done

# The Secret name is intentionally fixture-only. Its URL value never appears in
# chart values: the rendered deployment must mount its `url` key at the exact
# url_file consumed by the rendered Alertmanager receiver.
helm template deadman-fixture "$CHART_DIR" \
    --set monitoring.enabled=true \
    --set monitoring.alertmanager.webhookSecret.name=fixture-webhook \
    --show-only templates/configmap-monitoring.yaml \
    --show-only templates/deployment-monitoring.yaml >"$BASE/rendered.yaml" \
    || fail 'Helm did not render the monitoring resources'

python3 - "$BASE/rendered.yaml" "$BASE" <<'PY'
import http.client
import json
import os
import queue
import ssl
import subprocess
import sys
import threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

rendered_path, base = map(Path, sys.argv[1:])
rendered = rendered_path.read_text(encoding="utf-8")

# These strings are taken from the rendered config, rather than a duplicate
# receiver definition. Together they bind the test sender to Alertmanager's
# actual Watchdog route, receiver, Secret-backed URL file, and repeat cadence.
required_rendered_contract = (
    'receiver: incident-webhook',
    'group_by: [alertname]',
    '- matchers: [alertname="Watchdog"]',
    'receiver: incident-webhook',
    'repeat_interval: 1m',
    'webhook_configs: [{url_file: /etc/alertmanager-webhook/url, send_resolved: true}]',
    'secretName: "fixture-webhook"',
    'items: [{key: "url", path: url}]',
    'mountPath: /etc/alertmanager-webhook',
    '- alert: Watchdog',
    'expr: vector(1)',
    'for: 1m',
    'labels: {severity: none}',
)
for expected in required_rendered_contract:
    if expected not in rendered:
        raise SystemExit(f"rendered monitoring contract missing: {expected}")

# A new certificate/key pair belongs only to this fixture directory and is used
# only by the loopback receiver. It is never read from a cluster Secret.
cert, key = base / 'fixture-cert.pem', base / 'fixture-key.pem'
subprocess.run(
    [
        'openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
        '-keyout', str(key), '-out', str(cert), '-days', '1',
        '-subj', '/CN=localhost',
    ],
    check=True,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
os.chmod(key, 0o600)

class VirtualClock:
    def __init__(self):
        self.now = datetime(2026, 1, 1, tzinfo=timezone.utc)
    def advance(self, seconds):
        if seconds < 0:
            raise AssertionError('virtual time cannot go backwards')
        self.now += timedelta(seconds=seconds)
        return self.now
    def iso(self):
        return self.now.isoformat().replace('+00:00', 'Z')

class FakeHttpsReceiver:
    """Loopback-only receiver with dead-man state driven by VirtualClock."""
    def __init__(self, clock):
        self.clock = clock
        self.receipts = []
        self.pages = []
        self._requests = queue.Queue()
        receiver = self
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                if self.path != '/alertmanager-webhook':
                    self.send_error(404)
                    return
                length = int(self.headers.get('Content-Length', '0'))
                try:
                    payload = json.loads(self.rfile.read(length))
                except json.JSONDecodeError:
                    self.send_error(400)
                    return
                receiver._requests.put(payload)
                self.send_response(200)
                self.end_headers()
            def log_message(self, *_):
                pass
        self.httpd = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(certfile=cert, keyfile=key)
        self.httpd.socket = context.wrap_socket(self.httpd.socket, server_side=True)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()
    @property
    def url(self):
        return f'https://127.0.0.1:{self.httpd.server_port}/alertmanager-webhook'
    def accept_one(self):
        # send_webhook waits for the HTTP response, so this non-blocking read
        # cannot race the request and introduces no wall-clock timeout/sleep.
        try:
            payload = self._requests.get_nowait()
        except queue.Empty as error:
            raise AssertionError('fake HTTPS receiver did not accept webhook') from error
        self.validate_watchdog(payload)
        self.receipts.append((self.clock.now, payload))
    def validate_watchdog(self, payload):
        assert payload['version'] == '4'
        assert payload['status'] == 'firing'
        assert payload['receiver'] == 'incident-webhook'
        assert payload['groupLabels'] == {'alertname': 'Watchdog'}
        assert payload['commonLabels']['alertname'] == 'Watchdog'
        assert payload['commonLabels']['severity'] == 'none'
        assert len(payload['alerts']) == 1
        alert = payload['alerts'][0]
        assert alert['status'] == 'firing'
        assert alert['labels'] == {'alertname': 'Watchdog', 'severity': 'none'}
        assert alert['annotations']['summary'] == 'Continuous alert pipeline health signal.'
        assert alert['annotations']['runbook'] == 'server/docs/runbooks/incident-observability.md'
        assert alert['startsAt'] == self.clock.iso()
    def page_if_absent(self):
        if not self.receipts:
            raise AssertionError('cannot evaluate absence before a Watchdog receipt')
        silence = self.clock.now - self.receipts[-1][0]
        if silence > timedelta(minutes=5):
            page = {
                'type': 'synthetic_page',
                'reason': 'watchdog_absence',
                'receiver': 'incident-webhook',
                'alertname': 'Watchdog',
                'silence_seconds': int(silence.total_seconds()),
                'observed_at': self.clock.iso(),
            }
            self.pages.append(page)
            return page
        return None
    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join()

def send_webhook(url, clock):
    payload = {
        'version': '4', 'groupKey': '{}:{alertname="Watchdog"}',
        'status': 'firing', 'receiver': 'incident-webhook',
        'groupLabels': {'alertname': 'Watchdog'},
        'commonLabels': {'alertname': 'Watchdog', 'severity': 'none'},
        'commonAnnotations': {
            'summary': 'Continuous alert pipeline health signal.',
            'runbook': 'server/docs/runbooks/incident-observability.md',
        },
        'externalURL': 'http://alertmanager.fixture.invalid',
        'alerts': [{
            'status': 'firing',
            'labels': {'alertname': 'Watchdog', 'severity': 'none'},
            'annotations': {
                'summary': 'Continuous alert pipeline health signal.',
                'runbook': 'server/docs/runbooks/incident-observability.md',
            },
            'startsAt': clock.iso(), 'endsAt': '0001-01-01T00:00:00Z',
            'generatorURL': 'http://prometheus.fixture.invalid/graph?g0.expr=vector%281%29',
            'fingerprint': 'fixture-watchdog',
        }],
    }
    # The fixture certificate is deliberately untrusted outside this process;
    # disable verification only for this explicit 127.0.0.1 fixture connection.
    connection = http.client.HTTPSConnection('127.0.0.1', int(url.split(':')[2].split('/')[0]), context=ssl._create_unverified_context())
    connection.request('POST', '/alertmanager-webhook', json.dumps(payload), {'Content-Type': 'application/json'})
    response = connection.getresponse()
    assert response.status == 200
    response.read()
    connection.close()

clock = VirtualClock()
receiver = FakeHttpsReceiver(clock)
try:
    # Watchdog's rendered `for: 1m` and route repeat interval are represented by
    # exact one-minute virtual deliveries. No sleep or real scheduler is used.
    for expected_second in (60, 120, 180):
        clock.advance(60)
        assert int((clock.now - datetime(2026, 1, 1, tzinfo=timezone.utc)).total_seconds()) == expected_second
        send_webhook(receiver.url, clock)
        receiver.accept_one()
    receipt_seconds = [int((at - datetime(2026, 1, 1, tzinfo=timezone.utc)).total_seconds()) for at, _ in receiver.receipts]
    assert receipt_seconds == [60, 120, 180], receipt_seconds
    assert all(b - a == 60 for a, b in zip(receipt_seconds, receipt_seconds[1:]))

    # Stop Watchdog input. Advancing to 481 seconds is 301 virtual seconds after
    # the last accepted delivery: strictly beyond the five-minute dead-man limit.
    clock.advance(301)
    page = receiver.page_if_absent()
    assert page == {
        'type': 'synthetic_page', 'reason': 'watchdog_absence',
        'receiver': 'incident-webhook', 'alertname': 'Watchdog',
        'silence_seconds': 301, 'observed_at': '2026-01-01T00:08:01Z',
    }
    assert len(receiver.receipts) == 3
    assert receiver.pages == [page]
finally:
    receiver.close()

print('PASS: deadman_fixture::watchdog_absence')
PY

import threading

import pytest

from tarsier_perception.telemetry import STAGES, StageTelemetry


def test_stage_windows_reset_and_include_failed_calls(monkeypatch):
    clock = iter([0, 1, 1.02, 2, 2.04, 3, 5])
    monkeypatch.setattr('tarsier_perception.telemetry.time.perf_counter', lambda: next(clock))
    stats = StageTelemetry()
    with stats.measure('face'):
        pass
    with pytest.raises(ValueError), stats.measure('face'):
        raise ValueError('failed inference')
    sample = stats.snapshot()
    face = sample['stages']['face']
    assert sample['interval_ms'] == 3000
    assert face['calls'] == 2
    assert face['total_ms'] == pytest.approx(60)
    assert face['max_ms'] == pytest.approx(40)
    assert sample['stages']['depth']['max_ms'] is None
    empty = stats.snapshot()
    assert empty['interval_ms'] == 2000
    assert empty['stages']['face']['calls'] == 0
    assert len(empty['stages']) == len(STAGES)


def test_concurrent_updates_are_bounded_and_not_lost():
    stats = StageTelemetry()

    def work():
        for _ in range(1000):
            with stats.measure('face'):
                pass

    threads = [threading.Thread(target=work) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    sample = stats.snapshot()
    assert sample['stages']['face']['calls'] == 4000
    assert len(stats._values) == len(STAGES)


def test_publisher_survives_http_failure_and_stops_on_exit():
    import json
    import queue
    from http.server import BaseHTTPRequestHandler, HTTPServer

    received = queue.Queue()

    class Handler(BaseHTTPRequestHandler):
        calls = 0

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            received.put((self.path, body))
            Handler.calls += 1
            self.send_response(503 if Handler.calls == 1 else 204)
            self.end_headers()

        def log_message(self, *_):
            pass

    server = HTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    stats = StageTelemetry()
    try:
        with stats.publishing(f'http://127.0.0.1:{server.server_port}'):
            with stats.measure('face'):
                pass
            path, first = received.get(timeout=4)
            assert path == '/api/v1/perception/telemetry'
            assert first['stages']['face']['calls'] == 1
            with stats.measure('hands'):
                pass
            _, second = received.get(timeout=4)
            assert second['stages']['hands']['calls'] == 1
            assert second['stages']['face']['calls'] == 0
        assert not any(t.name == 'tarsier-metrics' for t in threading.enumerate())
    finally:
        server.shutdown()
        server.server_close()
        thread.join()

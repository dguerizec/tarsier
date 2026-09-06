import urllib.request

from tarsier_perception.auth import authorize


def test_daemon_request_uses_token_without_changing_payload(monkeypatch):
    monkeypatch.setenv("TARSIER_API_TOKEN", "test-client-token")
    request = urllib.request.Request(
        "http://127.0.0.1:8742/api/v1/perception/mask",
        data=b"mask",
        headers={"Content-Type": "application/octet-stream"},
    )
    assert authorize(request) is request
    assert request.get_header("Authorization") == "Bearer test-client-token"
    assert request.data == b"mask"
    assert request.get_header("Content-type") == "application/octet-stream"


def test_daemon_request_remains_anonymous_without_token(monkeypatch):
    monkeypatch.delenv("TARSIER_API_TOKEN", raising=False)
    request = urllib.request.Request("http://127.0.0.1:8742/api/v1/video/identity")
    assert authorize(request).get_header("Authorization") is None

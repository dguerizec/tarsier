"""Authenticate daemon requests without attaching credentials to model downloads."""
import os
import urllib.request


def authorize(request: urllib.request.Request) -> urllib.request.Request:
    token = os.environ.get("TARSIER_API_TOKEN")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    return request

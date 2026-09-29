import json
from types import SimpleNamespace

import httpx

from fantasy_manager.report.notify import any_failed, chunk_text, notify_all, send_discord, send_slack

DISCORD = "https://discord.com/api/webhooks/123/SECRET-TOKEN"
SLACK = "https://hooks.slack.com/services/T0/B0/SECRET"


def recorder(status=204, body=""):
    calls = []

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append((str(request.url), json.loads(request.content)))
        return httpx.Response(status, text=body)

    return calls, httpx.Client(transport=httpx.MockTransport(handler))


def test_chunk_text_respects_limit_and_lines():
    text = "\n".join(f"line {i} " + "x" * 90 for i in range(60))
    chunks = chunk_text(text, 1900)
    assert len(chunks) > 1 and all(len(c) <= 1900 for c in chunks)
    assert "\n".join(chunks) == text  # split only at line boundaries
    long_line = "word " * 1000
    parts = chunk_text(long_line, 1900)
    assert all(len(c) <= 1900 for c in parts) and len(parts) == 3
    assert chunk_text("   ", 100) == []


def test_send_discord_chunks_and_payload():
    calls, client = recorder(204)
    text = "\n".join("y" * 100 for _ in range(50))  # ~5050 chars
    res = send_discord(DISCORD, text, client=client)
    assert res == "discord: sent 3 message(s)"
    assert all(len(p["content"]) <= 1900 for _, p in calls)
    assert calls[0][1]["allowed_mentions"] == {"parse": []}
    assert calls[0][0] == DISCORD


def test_send_slack_payload():
    calls, client = recorder(200, "ok")
    assert send_slack(SLACK, "hello", client=client) == "slack: sent 1 message(s)"
    assert calls == [(SLACK, {"text": "hello"})]


def test_http_error_returns_string_without_secret():
    calls, client = recorder(404, '{"message": "Unknown Webhook"}')
    res = send_discord(DISCORD, "hi", client=client)
    assert res.startswith("discord: error HTTP 404") and "Unknown Webhook" in res
    assert "SECRET" not in res


def test_transport_error_never_raises():
    def boom(request):
        raise httpx.ConnectError("no route", request=request)

    client = httpx.Client(transport=httpx.MockTransport(boom))
    res = send_slack(SLACK, "hi", client=client)
    assert res.startswith("slack: error ConnectError") and "SECRET" not in res

    def slow(request):
        raise httpx.ReadTimeout("slow", request=request)

    res = send_discord(DISCORD, "hi", client=httpx.Client(transport=httpx.MockTransport(slow)))
    assert res == "discord: error timed out after 0/1 message(s)"


def test_rate_limit_retried_once():
    state = {"n": 0}

    def handler(request):
        state["n"] += 1
        if state["n"] == 1:
            return httpx.Response(429, json={"retry_after": 0})
        return httpx.Response(204)

    res = send_discord(DISCORD, "hi", client=httpx.Client(transport=httpx.MockTransport(handler)))
    assert res == "discord: sent 1 message(s)" and state["n"] == 2


def test_notify_all():
    calls, client = recorder(200)
    settings = SimpleNamespace(discord_webhook_url=DISCORD, slack_webhook_url=SLACK)
    res = notify_all(settings, "digest", client=client)
    assert res == ["discord: sent 1 message(s)", "slack: sent 1 message(s)"]
    assert not any_failed(res)
    assert [u for u, _ in calls] == [DISCORD, SLACK]
    none = notify_all(SimpleNamespace(discord_webhook_url=None, slack_webhook_url=None), "x")
    assert len(none) == 1 and "no webhooks configured" in none[0]
    assert send_discord(None, "x") == "discord: not configured"

"""Discord / Slack incoming-webhook notifications.

All functions return human-readable result strings and never raise. Webhook URLs are secrets,
so they never appear in results or logs.
"""
from __future__ import annotations

import logging
import time
from typing import Any

import httpx

log = logging.getLogger(__name__)

TIMEOUT = 10.0
DISCORD_CHUNK = 1900   # Discord hard limit is 2000 chars per message
SLACK_CHUNK = 3900     # Slack truncates message text around 4000 chars
MAX_RETRY_AFTER = 5.0


def chunk_text(text: str, size: int) -> list[str]:
    """Split into chunks of at most ``size`` chars, preferring line boundaries."""
    text = (text or "").strip()
    if not text:
        return []
    chunks: list[str] = []
    cur = ""
    for line in text.splitlines():
        while len(line) > size:  # hard-wrap an over-long line
            if cur:
                chunks.append(cur)
                cur = ""
            cut = line.rfind(" ", 0, size)
            cut = cut if cut > size // 2 else size
            chunks.append(line[:cut].rstrip())
            line = line[cut:].lstrip()
        candidate = f"{cur}\n{line}" if cur else line
        if len(candidate) > size:
            chunks.append(cur)
            cur = line
        else:
            cur = candidate
    if cur.strip():
        chunks.append(cur)
    return [c for c in chunks if c.strip()]


def _client(client: httpx.Client | None) -> tuple[httpx.Client, bool]:
    if client is not None:
        return client, False
    return httpx.Client(timeout=TIMEOUT), True


def _post(client: httpx.Client, url: str, payload: dict) -> httpx.Response:
    resp = client.post(url, json=payload, timeout=TIMEOUT)
    if resp.status_code == 429:  # honour a short Retry-After once
        try:
            wait = float(resp.headers.get("retry-after") or resp.json().get("retry_after") or 1.0)
        except Exception:
            wait = 1.0
        if wait <= MAX_RETRY_AFTER:
            time.sleep(max(0.0, wait))
            resp = client.post(url, json=payload, timeout=TIMEOUT)
    return resp


def _send_chunks(name: str, url: str | None, chunks: list[str], make_payload,
                 client: httpx.Client | None) -> str:
    if not url:
        return f"{name}: not configured"
    if not chunks:
        return f"{name}: nothing to send"
    http, owned = _client(client)
    sent = 0
    try:
        for chunk in chunks:
            resp = _post(http, url, make_payload(chunk))
            if resp.status_code >= 400:
                body = " ".join(resp.text.split())[:200]
                return f"{name}: error HTTP {resp.status_code} after {sent}/{len(chunks)} message(s): {body}"
            sent += 1
    except httpx.TimeoutException:
        return f"{name}: error timed out after {sent}/{len(chunks)} message(s)"
    except httpx.HTTPError as exc:
        return f"{name}: error {type(exc).__name__} after {sent}/{len(chunks)} message(s)"
    except Exception as exc:  # never raise to callers
        log.exception("%s notification failed", name)
        return f"{name}: error {type(exc).__name__}"
    finally:
        if owned:
            http.close()
    return f"{name}: sent {sent} message(s)"


def send_discord(webhook_url: str | None, text: str, client: httpx.Client | None = None) -> str:
    """POST ``{"content": ...}`` chunks (<= 1900 chars) to a Discord webhook. Mentions are disabled."""
    return _send_chunks("discord", webhook_url, chunk_text(text, DISCORD_CHUNK),
                        lambda c: {"content": c, "allowed_mentions": {"parse": []}}, client)


def send_slack(webhook_url: str | None, text: str, client: httpx.Client | None = None) -> str:
    """POST ``{"text": ...}`` chunks to a Slack incoming webhook."""
    return _send_chunks("slack", webhook_url, chunk_text(text, SLACK_CHUNK),
                        lambda c: {"text": c}, client)


def notify_all(settings: Any, text: str, client: httpx.Client | None = None) -> list[str]:
    """Send to every configured webhook; returns one result string per configured target."""
    results: list[str] = []
    discord = getattr(settings, "discord_webhook_url", None)
    slack = getattr(settings, "slack_webhook_url", None)
    if discord:
        results.append(send_discord(discord, text, client=client))
    if slack:
        results.append(send_slack(slack, text, client=client))
    if not results:
        results.append("no webhooks configured (set DISCORD_WEBHOOK_URL and/or SLACK_WEBHOOK_URL)")
    return results


def any_failed(results: list[str]) -> bool:
    return any(": error" in r for r in results)


__all__ = ["send_discord", "send_slack", "notify_all", "chunk_text", "any_failed"]


# --------------------------------------------------------------------------- short change alerts

CHANGES_CHUNK = 1500   # phone-sized messages (the pre-game run, fantasy_manager.pregame)
BULLET = "•"   # a bullet point


def format_changes(title: str, lines: list[str], style: str = "discord") -> str:
    """``title`` in bold (Discord ``**``, Slack ``*``) then one bullet per line; blank lines dropped."""
    bold = "**" if style == "discord" else "*"
    head = f"{bold}{title.strip()}{bold}" if title and title.strip() else ""
    body = [f"{BULLET} {' '.join(str(ln).split())}" for ln in lines or [] if str(ln).strip()]
    return "\n".join([head, *body] if head else body)


def notify_changes(settings: Any, title: str, lines: list[str], client: httpx.Client | None = None) -> list[str]:
    """Post a short bulleted alert (title + one bullet per line) to every configured webhook, in
    messages of at most 1500 chars (split on bullet boundaries). Same result strings as
    :func:`notify_all`; never raises."""
    results: list[str] = []
    discord = getattr(settings, "discord_webhook_url", None)
    slack = getattr(settings, "slack_webhook_url", None)
    if discord:
        results.append(_send_chunks("discord", discord, chunk_text(format_changes(title, lines, "discord"),
                                                                   CHANGES_CHUNK),
                                    lambda c: {"content": c, "allowed_mentions": {"parse": []}}, client))
    if slack:
        results.append(_send_chunks("slack", slack, chunk_text(format_changes(title, lines, "slack"), CHANGES_CHUNK),
                                    lambda c: {"text": c}, client))
    if not results:
        results.append("no webhooks configured (set DISCORD_WEBHOOK_URL and/or SLACK_WEBHOOK_URL)")
    return results


__all__ += ["notify_changes", "format_changes"]

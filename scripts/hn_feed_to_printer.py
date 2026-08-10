#!/usr/bin/env python3
"""Sync HN Buzzing feed items to the printer endpoint."""

from __future__ import annotations

import hashlib
import html
import os
import re
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable
from xml.etree import ElementTree as ET


FEED_URL = "https://hn.buzzing.cc/feed.xml"
PRINTER_URL = "https://printer.199478.xyz/print"
PRINTER_USER_AGENT = "Mozilla/5.0 (compatible; RSSPrinter/1.0)"
SCRIPT_DIR = Path(__file__).resolve().parent
STATE_PATH = SCRIPT_DIR / ".hn_feed_state"
MAX_ENTRIES = 10


@dataclass(frozen=True)
class FeedItem:
    title: str
    summary: str
    identifier: str


def local_name(tag: str) -> str:
    """Return XML tag local name to ignore namespace prefixes."""
    return tag.rsplit("}", 1)[-1]


def clean_text(value: str | None) -> str:
    if value is None:
        return ""
    text = html.unescape(value)
    text = re.sub(r"<[^>]+>", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text


def parse_feed(xml_text: str) -> list[FeedItem]:
    root = ET.fromstring(xml_text)
    entries: list[FeedItem] = []

    for element in root.iter():
        tag = local_name(element.tag)
        if tag not in {"item", "entry"}:
            continue

        title = ""
        summary = ""
        identifier = ""

        for child in element:
            child_tag = local_name(child.tag)
            if child_tag == "title" and not title:
                title = child.text or ""
            elif child_tag in {"description", "summary", "content"} and not summary:
                summary = child.text or ""
            elif child_tag in {"guid", "id", "link"} and not identifier:
                identifier = child.text or ""

        if not title:
            continue

        if not identifier and element.get("id"):
            identifier = element.get("id") or ""

        if not identifier:
            identifier = hashlib.sha1(f"{title}\n{summary}".encode("utf-8")).hexdigest()

        entries.append(
            FeedItem(
                title=clean_text(title),
                summary=clean_text(summary),
                identifier=identifier,
            )
        )

    return entries


def load_state() -> set[str]:
    if not STATE_PATH.exists():
        return set()

    lines = [line.strip() for line in STATE_PATH.read_text(encoding="utf-8").splitlines()]
    return {line for line in lines if line}


def save_state(identifiers: Iterable[str]) -> None:
    STATE_PATH.write_text("\n".join(sorted(set(identifiers))) + "\n", encoding="utf-8")


def render_markdown(items: list[FeedItem]) -> str:
    blocks: list[str] = []
    for item in items:
        lines = [f"## {item.title}", "", item.summary or "(No summary)"]
        blocks.append("\n".join(lines))

    return "\n\n".join(blocks).strip()


def fetch_feed() -> str:
    req = urllib.request.Request(
        FEED_URL,
        headers={
            "User-Agent": "hn-feed-printer/1.0",
            "Accept": "application/rss+xml, application/xml, text/xml;q=0.9, */*;q=0.8",
        },
    )

    with urllib.request.urlopen(req, timeout=20) as response:
        payload = response.read()

    return payload.decode("utf-8", errors="replace")


def post_markdown(content: str) -> None:
    data = content.encode("utf-8")
    headers = {
        "User-Agent": PRINTER_USER_AGENT,
        "Accept": "text/markdown, text/plain, */*",
        "Content-Type": "text/markdown; charset=utf-8",
        "Content-Length": str(len(data)),
    }
    auth = os.environ.get("PRINTER_TOKEN") or os.environ.get("PRINT_AUTH_TOKEN")
    if auth:
        headers["Authorization"] = f"Bearer {auth}"

    req = urllib.request.Request(
        PRINTER_URL,
        method="POST",
        data=data,
        headers=headers,
    )

    with urllib.request.urlopen(req, timeout=30) as response:
        response.read()
        status = response.getcode()

    print(f"Posted to printer: HTTP {status}")


def main() -> int:
    try:
        xml_text = fetch_feed()
        feed_items = parse_feed(xml_text)
    except (ET.ParseError, urllib.error.URLError, OSError) as exc:
        print(f"Feed fetch/parse failed: {exc}")
        return 1

    if not feed_items:
        print("No entries found in feed.")
        return 0

    seen = load_state()
    new_items = [item for item in feed_items[:MAX_ENTRIES] if item.identifier not in seen]

    if not new_items:
        print("No new entries to print.")
        return 0

    markdown = render_markdown(new_items)
    if not markdown:
        print("No printable markdown generated.")
        return 0

    try:
        post_markdown(markdown)
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        message = body.strip()[:1000] if body else exc.reason
        print(f"Printer request failed: HTTP Error {exc.code}: {message}")
        return 1
    except (urllib.error.URLError, OSError) as exc:
        print(f"Printer request failed: {exc}")
        return 1

    save_state(seen.union(item.identifier for item in new_items))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

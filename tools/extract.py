#!/usr/bin/env python3
"""Turn a URL, HTML file or PDF into clean markdown for a Lumbridge Docs clipping.

Usage: extract.py <url-or-path>   → prints JSON {title, author, date, url, sitename, markdown, kind}
Runs in a venv with trafilatura and pypdf (`[clipper] python`). lumbridge-libraryd calls it for docs_clip when the
caller gives a URL but no text. Untrusted input: never executes anything from the page.
"""
import json
import subprocess
import sys
import urllib.request

MAX_BYTES = 40 * 1024 * 1024
UA = "Mozilla/5.0 (X11; Linux) lumbridge-library-clipper/1.0"


def fetch(url):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=45) as r:
        ctype = r.headers.get("content-type", "")
        data = r.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise SystemExit("too large")
    return data, ctype


def pdf_markdown(data):
    try:  # poppler-utils' pdftotext when installed, else pypdf
        p = subprocess.run(["pdftotext", "-layout", "-", "-"], input=data, capture_output=True, timeout=120)
        if p.returncode == 0 and p.stdout.strip():
            return p.stdout.decode("utf-8", "replace")
    except FileNotFoundError:
        pass
    import io
    from pypdf import PdfReader
    return "\n\n".join((pg.extract_text() or "") for pg in PdfReader(io.BytesIO(data)).pages)


def main():
    src = sys.argv[1]
    is_url = src.startswith(("http://", "https://"))
    if is_url:
        data, ctype = fetch(src)
    else:
        data, ctype = open(src, "rb").read(), ""
    out = {"url": src if is_url else None, "title": None, "author": None, "date": None, "sitename": None}
    if "pdf" in ctype or data[:5] == b"%PDF-":
        out.update(kind="pdf", markdown=pdf_markdown(data))
    else:
        import trafilatura
        html = data.decode("utf-8", "replace")
        md = trafilatura.extract(html, url=src if is_url else None, output_format="markdown",
                                 include_tables=True, include_links=True, include_images=False,
                                 favor_recall=True)
        meta = trafilatura.extract_metadata(html, default_url=src if is_url else None)
        if meta:
            out.update(title=meta.title, author=meta.author, date=meta.date, sitename=meta.sitename)
        out.update(kind="html", markdown=md or "")
    if not out["markdown"].strip():
        raise SystemExit("no extractable text")
    json.dump(out, sys.stdout, ensure_ascii=False)


if __name__ == "__main__":
    main()

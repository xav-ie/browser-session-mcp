#!/usr/bin/env python3
"""Fail when a commit message in the given range is too long.

History gets read by people scanning for what changed; rationale beyond a
sentence or two belongs in the PR or the docs. Adapted from xav-ie/Tetherbug.

    check-commit-messages.py <base>..<head>
"""

import re
import subprocess
import sys

SUMMARY_MAX_CHARS = 72  # first line
BODY_MAX_LINES = 6  # non-empty lines after the first
BODY_MAX_CHARS = 500  # total, after the first line
TRAILER = re.compile(r"^[A-Za-z-]+:\s")  # Co-authored-by: and such are metadata


def git(*args):
    return subprocess.run(
        ["git", *args], capture_output=True, text=True, check=True
    ).stdout


def main(rev_range):
    shas = git("rev-list", "--no-merges", rev_range).split()
    problems = []
    for sha in shas:
        author, message = git("log", "-1", "--format=%an%x00%B", sha).split("\0", 1)
        if author.endswith("[bot]"):  # Dependabot and friends write their own
            continue
        summary, *rest = message.strip().split("\n")
        body = [l for l in rest if l.strip() and not TRAILER.match(l.strip())]
        label = f"{sha[:7]} {summary}"
        if len(summary) > SUMMARY_MAX_CHARS:
            problems.append(f"{label}: summary is {len(summary)} chars (max {SUMMARY_MAX_CHARS})")
        if len(body) > BODY_MAX_LINES:
            problems.append(f"{label}: body is {len(body)} lines (max {BODY_MAX_LINES})")
        if len("\n".join(body)) > BODY_MAX_CHARS:
            problems.append(f"{label}: body is {len(chr(10).join(body))} chars (max {BODY_MAX_CHARS})")
    print("\n".join(problems) or f"{len(shas)} commit message(s) OK")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))

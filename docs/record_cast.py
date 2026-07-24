#!/usr/bin/env python3
"""Captures `pact demo`'s real, live stdout -- real content, real relative
ordering, real git-worktree-driven pauses -- into an asciicast v2 .cast
file, for `agg` (https://github.com/asciinema/agg) to render into a GIF.

asciinema's own recorder can't run on native Windows Python: it
unconditionally imports the Unix-only `fcntl` module at startup and fails
before doing anything else. This script builds the .cast directly from a
real subprocess run instead of going through asciinema's recorder, so the
result is still genuinely captured output, not hand-authored -- see
DESIGN.md ("pact-cli > demo GIF re-recording", issue #124) for the full
tooling story, including why the earlier version of this file
(`render_demo.py`, since deleted) existed at all.

`pact demo` itself finishes in about 1.5 real seconds, so the real gaps
between lines are mostly a few milliseconds -- too fast to read. Real
relative ordering and real longer pauses (e.g. around the two actual
`git worktree add` calls) are preserved verbatim; a MIN_HOLD_SECONDS floor
is applied only to raise unreadably-short real gaps, never to shorten a
real one. Disclosed explicitly in the README, same as the tradeoff the
previous Pillow-based approach disclosed.
"""
import json
import subprocess
import sys
import time

COLS = 110
ROWS = 39
MIN_HOLD_SECONDS = 0.35


def main() -> None:
    pact_exe = sys.argv[1] if len(sys.argv) > 1 else "pact"
    out_path = sys.argv[2] if len(sys.argv) > 2 else "demo.cast"

    proc = subprocess.Popen(
        [pact_exe, "demo"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=1,
        universal_newlines=True,
    )

    start = time.time()
    captured = []
    for line in proc.stdout:
        captured.append((time.time() - start, line))
    proc.wait()
    if proc.returncode != 0:
        print(f"pact demo exited {proc.returncode}", file=sys.stderr)
        sys.exit(1)

    events = [(0.0, "$ pact demo\r\n")]
    last_t = 0.0
    for real_t, line in captured:
        t = max(real_t, last_t + MIN_HOLD_SECONDS)
        events.append((t, line.rstrip("\n") + "\r\n"))
        last_t = t
    events.append((last_t + 2.0, ""))  # hold the final frame before agg's own loop/end

    with open(out_path, "w", encoding="utf-8") as f:
        header = {
            "version": 2,
            "width": COLS,
            "height": ROWS,
            "timestamp": int(start),
            "env": {"TERM": "xterm-256color"},
        }
        f.write(json.dumps(header) + "\n")
        for t, data in events:
            f.write(json.dumps([round(t, 6), "o", data]) + "\n")

    print(f"wrote {out_path} ({len(events)} events, {last_t:.2f}s)")


if __name__ == "__main__":
    main()

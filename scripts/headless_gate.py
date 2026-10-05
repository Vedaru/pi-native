#!/usr/bin/env python3
"""Headless-worker dependency gate.

A swarm unit draws no terminal and needs no clipboard or GUI. It may decode
images: the `read` tool attaches them to the model, which is agent input, not
rendering.

Names are matched exactly, not as substrings: `webpki-roots` is a TLS
dependency, not the `webp` image codec, and this gate must not confuse them.

Usage:
    python3 scripts/headless_gate.py                 # check pipelets
    python3 scripts/headless_gate.py -p some-worker  # check a split worker
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

# Crates whose only reason to exist is interactive or graphical output. A
# headless worker links none of them.
FORBIDDEN = frozenset(
    {
        # Terminal / TUI renderers
        "crossterm",
        "ratatui",
        "termion",
        "termwiz",
        "tui",
        "tuirealm",
        "notcurses",
        "ncurses",
        "pancurses",
        "cursive",
        "tui-input",
        "pi-tui",
        # Clipboards
        "arboard",
        "clipboard",
        "copypasta",
        "cli-clipboard",
        "x11-clipboard",
        # Terminal image renderers (display-only; image *decoding* is allowed,
        # because `read` attaches images to the model)
        "viuer",
        "sixel",
        "kitty",
        # GUI toolkits / GPU
        "egui",
        "eframe",
        "iced",
        "slint",
        "gtk",
        "gtk4",
        "wgpu",
        "winit",
        "sdl2",
    }
)

# `cargo tree --prefix none` prints one crate per line: `name vX.Y.Z`, optionally
# followed by `(*)` (already shown) or `(proc-macro)`.
LINE = re.compile(r"^([A-Za-z0-9_-]+) v\d")


def dependency_names(package: str) -> set[str]:
    result = subprocess.run(
        ["cargo", "tree", "-p", package, "-e", "normal", "--prefix", "none"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise SystemExit(f"cargo tree failed for package {package!r}")

    names = set()
    for line in result.stdout.splitlines():
        match = LINE.match(line.strip())
        if match:
            names.add(match.group(1))
    return names


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "-p",
        "--package",
        default="pipelets",
        help="package to check (default: pipelets)",
    )
    args = parser.parse_args()

    names = dependency_names(args.package)
    found = sorted(names & FORBIDDEN)
    if found:
        print(
            f"✗ {args.package} depends on UI-only crates: {', '.join(found)}",
            file=sys.stderr,
        )
        print(
            "  A headless swarm worker must not link a TUI, clipboard, or GUI. "
            "Image decoding is allowed (agent input); rendering is not. Move a "
            "display dependency to a UI crate that depends on core, not into the worker.",
            file=sys.stderr,
        )
        return 1

    print(f"✓ {args.package} is headless ({len(names)} crates, no UI-only deps)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

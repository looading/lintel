#!/usr/bin/env python3
"""Compatibility entry point for `lintel prepare`."""

import os
from pathlib import Path


if __name__ == "__main__":
    launcher = Path(__file__).resolve().with_name("lintel.sh")
    os.execv(launcher, [str(launcher), "prepare", "--lintel-path", str(launcher.parents[1])])

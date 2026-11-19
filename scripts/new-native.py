#!/usr/bin/env python3
"""Compatibility entry point for `lintel new`."""

import os
from pathlib import Path
import sys


if __name__ == "__main__":
    launcher = Path(__file__).resolve().with_name("lintel.sh")
    os.execv(launcher, [str(launcher), "new", *sys.argv[1:]])

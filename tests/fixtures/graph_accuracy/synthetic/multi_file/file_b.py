"""file_b: provides helpers imported by file_a.

Synthetic two-file fixture for Spike B' BUG #2 validation: cross-file
imports should resolve to the defining file's symbols, not loop back to
the importing file.
"""

from __future__ import annotations


def helper(x: int) -> int:
    return x * 2


def square(y: int) -> int:
    return y * y

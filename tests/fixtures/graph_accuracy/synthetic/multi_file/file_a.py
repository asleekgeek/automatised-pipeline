"""file_a: imports from file_b and calls its functions.

Synthetic two-file fixture for Spike B' BUG #2 validation: this file
should produce resolved Imports edges to file_b's symbols and resolved
Calls edges to helper/square through the import.
"""

from __future__ import annotations
from file_b import helper, square


def main() -> int:
    return helper(5) + square(3)

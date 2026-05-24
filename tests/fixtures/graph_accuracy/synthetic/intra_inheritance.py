"""Synthetic fixture: intra-file class inheritance.

Validates Spike B' BUG #9 fix: the resolver must emit Extends_Struct_Struct
edges when a class derives from another class in the same file. Real Cortex
files don't exhibit this pattern (every class either has no base or extends
something from another file), so this fixture is purpose-built.
"""

from __future__ import annotations


class Animal:
    def speak(self) -> str:
        return ""


class Dog(Animal):
    def speak(self) -> str:
        return "woof"


class Puppy(Dog):
    def speak(self) -> str:
        return "yip"

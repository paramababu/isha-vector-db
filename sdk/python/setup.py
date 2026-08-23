"""Wheel tagging.

Everything about this package is declared in `pyproject.toml`. This file exists for one thing
that cannot be expressed there: making the wheel platform-specific.

setuptools decides portability by looking for compiled extension modules. There are none — the
binding is ctypes over a shared library that is copied in — so left alone it tags the wheel
`py3-none-any`, and pip will then install it on any machine, including one whose architecture the
bundled library does not match. The failure arrives as an `OSError` at import time on somebody
else's laptop, which is the worst possible place to find a packaging mistake.
"""

from setuptools import setup
from setuptools.dist import Distribution

try:  # setuptools >= 70 moved it out of the `wheel` package
    from setuptools.command.bdist_wheel import bdist_wheel
except ImportError:  # pragma: no cover - older toolchains
    from wheel.bdist_wheel import bdist_wheel

import pathlib


def bundles_a_library() -> bool:
    """Whether this build has a shared library to carry.

    False when building from a source checkout with no library copied in, where the binding falls
    back to finding a local `cargo build`. That wheel is genuinely portable, and tagging it
    otherwise would be its own lie.
    """
    package = pathlib.Path(__file__).parent / "isha_vector_db"
    return any(package.glob(f"*{s}") for s in (".so", ".dylib", ".dll"))


class Binary(Distribution):
    def has_ext_modules(self) -> bool:
        return bundles_a_library()


class Wheel(bdist_wheel):
    def finalize_options(self) -> None:
        super().finalize_options()
        self.root_is_pure = not bundles_a_library()

    def get_tag(self):
        python, abi, platform = super().get_tag()
        if not bundles_a_library():
            return python, abi, platform
        # ctypes has no ABI of its own, so one wheel serves every Python 3.9+ on this platform.
        # Pinning an interpreter version here would mean building the same bytes eight times.
        return "py3", "none", platform


setup(distclass=Binary, cmdclass={"bdist_wheel": Wheel})

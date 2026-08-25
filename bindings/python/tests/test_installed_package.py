"""Installed-wheel checks: the bundled cdylib wins over any system copy.

Only meaningful against the INSTALLED package (site-packages) where the
wheel placed the cdylib next to `jqf`. Under the checkout layout (the
bindings-python gate runs `target/release`) there is no bundled copy and
this suite is skipped by construction.
"""
import os
import unittest

import jqf
from jqf import _ffi

_BUNDLED = any(
    os.path.exists(os.path.join(os.path.dirname(jqf.__file__), name))
    for name in _ffi._LIB_NAMES
)


@unittest.skipUnless(_BUNDLED, "checkout layout: no bundled cdylib")
class InstalledPackageLoadsBundledCdylib(unittest.TestCase):
    def test_the_loaded_library_is_inside_the_package(self):
        # `_name` is the path ctypes actually loaded; it must be the wheel-bundled copy, never a bare soname resolved
        # from the system.
        name = _ffi._lib._name
        pkgdir = os.path.realpath(os.path.dirname(jqf.__file__))
        self.assertTrue(os.path.isabs(name), name)
        self.assertTrue(os.path.realpath(name).startswith(pkgdir), name)

    def test_import_and_a_real_run_work(self):
        self.assertEqual(jqf.run("1+1").output, b"2\n")

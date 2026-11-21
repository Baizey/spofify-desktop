#!/usr/bin/env python3
"""Check the fixed desktop roles and the packaged standalone brand artwork."""
from pathlib import Path
import re
import unittest
from xml.etree import ElementTree

ROOT = Path(__file__).resolve().parents[1]
SVG = "{http://www.w3.org/2000/svg}"


class FixedTheme(unittest.TestCase):
    def test_roles_are_fixed(self):
        source = (ROOT / "ui/theme.slint").read_text()
        theme = source.split("export global Theme {", 1)[1].split("}", 1)[0]
        roles = dict(re.findall(r"out property <color> ([\w-]+): (#[0-9a-f]{6});", theme))
        self.assertEqual(roles, {
            "bg": "#131617", "rail": "#191d1e", "panel": "#1e2324", "raised": "#293032",
            "line": "#303638", "text": "#f4f6fa", "muted": "#b4bdcb", "subtle": "#9ca9ba",
            "primary": "#66e3b4", "on-primary": "#08271d", "tertiary": "#dfbd78", "error": "#ffa2ac",
        })
        self.assertEqual(theme.count("property <color>"), len(roles))

    def test_packaged_brand_is_only_four_mint_bars(self):
        logo = ElementTree.parse(ROOT / "assets/spofify-logo.svg").getroot()
        self.assertEqual(logo.tag, SVG + "svg")
        self.assertEqual(logo.attrib, {"width": "108", "height": "108", "viewBox": "0 0 108 108"})
        self.assertEqual(len(logo), 1)
        self.assertEqual(logo[0].tag, SVG + "path")
        self.assertEqual(logo[0].attrib, {
            "fill": "#66e3b4", "d": "M28,38h8v32h-8zM43,25h8v58h-8zM58,34h8v40h-8zM73,43h8v22h-8z",
        })


if __name__ == "__main__":
    unittest.main()

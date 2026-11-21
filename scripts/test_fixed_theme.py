#!/usr/bin/env python3
"""Check the fixed desktop surface roles and shared glyph artwork."""
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

    def test_control_and_container_corners(self):
        theme = (ROOT / "ui/theme.slint").read_text()
        menu = (ROOT / "ui/overflow-menu.slint").read_text()
        self.assertIn("out property <length> control-radius: 8px;", theme)
        self.assertIn("out property <length> container-radius: 12px;", theme)
        self.assertIn("border-radius: round ? self.height / 2 : Theme.control-radius;", theme)
        self.assertIn("border-radius: Theme.control-radius;", theme)
        self.assertIn("border-width: 1px; border-radius: Theme.container-radius;", menu)

    def test_shared_glyphs_have_a_24_unit_viewport(self):
        names = ("play", "pause", "prev", "next", "queue", "playlists", "devices", "search",
                 "settings", "history", "shuffle", "repeat", "repeat-one", "lyrics", "album",
                 "artist", "note", "more")
        for name in names:
            with self.subTest(name=name):
                self.assertEqual(self.icon(name).attrib["viewBox"], "0 0 24 24")

    def test_more_is_horizontal_and_playlists_is_distinct_from_queue(self):
        dots = self.icon("more").findall(".//" + SVG + "circle")
        self.assertEqual([(dot.get("cx"), dot.get("cy")) for dot in dots],
                         [("4", "12"), ("12", "12"), ("20", "12")])
        playlists = self.icon("playlists")
        path = playlists.find(SVG + "path").attrib["d"]
        self.assertEqual(path, "M4 5h16M4 11h16M4 17h10")
        self.assertEqual(playlists.attrib["stroke-width"], "1.7")
        self.assertEqual(playlists.attrib["stroke-linecap"], "round")
        self.assertNotEqual(self.icon("queue").find(SVG + "path").attrib["d"], path)

    @staticmethod
    def icon(name):
        return ElementTree.parse(ROOT / "assets/icons" / f"{name}.svg").getroot()

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

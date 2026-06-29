#!/usr/bin/env python3
# Parse src/tui/theme.rs PALETTES -> mdBook variables.css.
# One mdBook theme per TUI palette; colors lifted verbatim so docs == mock (Alt+P).
import re, pathlib

SRC = pathlib.Path("/home/dio/sir-bone-rs/src/tui/theme.rs")
OUT = pathlib.Path("/home/dio/sir-bone-rs/docs-site/book/theme/css/variables.css")

FIELDS = ["accent", "fg", "muted", "success", "err", "border", "bg", "info", "purple"]
txt = SRC.read_text()

pal = {}
for m in re.finditer(r"pub const (\w+): Palette = Palette \{([^}]*)\};", txt):
    name, body = m.group(1), m.group(2)
    pal[name] = {}
    for f in FIELDS:
        mm = re.search(rf"\b{f}:\s*rgb\((\d+),\s*(\d+),\s*(\d+)\)", body)
        pal[name][f] = tuple(int(x) for x in mm.groups())

order_m = re.search(r"pub const PALETTES.*?&\[(.*?)\];", txt, re.S)
order = re.findall(r'\("([^"]+)",\s*(\w+)\)', order_m.group(1))  # [(css_class, CONST)]

def hx(t): return "#{:02X}{:02X}{:02X}".format(*t)
def rgba(t, a): return "rgba({}, {}, {}, {})".format(*t, a)

COPY = "invert(45%) sepia(6%) saturate(621%) hue-rotate(198deg) brightness(99%) contrast(85%)"
COPY_HOV = "invert(68%) sepia(55%) saturate(531%) hue-rotate(341deg) brightness(104%) contrast(101%)"

def vars_block(P):
    return f"""    --bg: {hx(P['bg'])};
    --fg: {hx(P['fg'])};

    --sidebar-bg: {hx(P['bg'])};
    --sidebar-fg: {hx(P['fg'])};
    --sidebar-non-existant: {hx(P['muted'])};
    --sidebar-active: {hx(P['accent'])};
    --sidebar-spacer: {hx(P['border'])};

    --scrollbar: {hx(P['muted'])};

    --icons: {hx(P['muted'])};
    --icons-hover: {hx(P['fg'])};

    --links: {hx(P['info'])};
    --inline-code-color: {hx(P['accent'])};

    --theme-popup-bg: {hx(P['bg'])};
    --theme-popup-border: {hx(P['border'])};
    --theme-hover: {hx(P['border'])};

    --quote-bg: {hx(P['border'])};
    --quote-border: {hx(P['accent'])};

    --warning-border: {hx(P['accent'])};

    --table-border-color: {hx(P['border'])};
    --table-header-bg: {hx(P['border'])};
    --table-alternate-bg: {hx(P['bg'])};

    --searchbar-border-color: {hx(P['border'])};
    --searchbar-bg: {hx(P['bg'])};
    --searchbar-fg: {hx(P['fg'])};
    --searchbar-shadow-color: {hx(P['accent'])};
    --searchresults-header-fg: {hx(P['muted'])};
    --searchresults-border-color: {hx(P['border'])};
    --searchresults-li-bg: {hx(P['bg'])};
    --search-mark-bg: {hx(P['accent'])};

    --color-scheme: dark;

    --copy-button-filter: {COPY};
    --copy-button-filter-hover: {COPY_HOV};

    --footnote-highlight: {hx(P['info'])};

    --overlay-bg: {rgba(P['bg'], 0.4)};

    --blockquote-note-color: {hx(P['info'])};
    --blockquote-tip-color: {hx(P['success'])};
    --blockquote-important-color: {hx(P['purple'])};
    --blockquote-warning-color: {hx(P['accent'])};
    --blockquote-caution-color: {hx(P['err'])};

    --sidebar-header-border-color: {hx(P['accent'])};"""

HEADER = """/* mdBook theme variables.
 *
 * :root carries the sirbone brand palette (default theme + "Auto" fallback,
 * since Auto applies a class with no matching CSS and falls through to :root).
 * Each .{name} block below is one TUI palette, switched from the theme picker.
 * Colors are lifted verbatim from src/tui/theme.rs so the docs and the mock's
 * Alt+P cycle match exactly.
 *
 * Regenerate: uv run python /tmp/gen_palettes.py
 */
"""

out = [HEADER]

# :root = structural vars + sirbone palette.
out.append(":root {\n")
out.append("""    --sidebar-target-width: 300px;
    --sidebar-width: min(var(--sidebar-target-width), 80vw);
    --sidebar-resize-indicator-width: 8px;
    --sidebar-resize-indicator-space: 2px;
    --page-padding: 15px;
    --content-max-width: 750px;
    --menu-bar-height: 50px;
    --mono-font: "Source Code Pro", Consolas, "Ubuntu Mono", Menlo, "DejaVu Sans Mono", monospace, monospace;
    --code-font-size: 0.875em;
    --searchbar-margin-block-start: 5px;

""")
sirbone = next(c for n, c in order if n == "sirbone")
out.append(vars_block(pal[sirbone]))
out.append("\n}\n\n")

for css_name, const in order:
    if css_name == "sirbone":
        continue
    out.append(f".{css_name} {{\n")
    out.append(vars_block(pal[const]))
    out.append("\n}\n\n")

OUT.parent.mkdir(parents=True, exist_ok=True)
OUT.write_text("".join(out))
print(f"wrote {OUT}; {len(order)} palettes: {[n for n,_ in order]}")

#!/usr/bin/env python3
"""Keeps the translations in step with the markup, without `slint-tr-extractor`.

`tr.py pot`   writes crates/uplink/lang/uplink.pot from every `@tr` in crates/uplink/ui.
`tr.py check` lists, per language, strings the markup has that its .po lacks or leaves empty,
              and entries the markup no longer has; the same for Android's values-<lang>/
              against values/; and any translation whose placeholders differ from its
              source's. Exits non-zero if any are found.

Two catalogues, one per runtime: the window's words are Slint's (.po), everything outside it —
notifications, the share sheet, the identity card — is Android's resources, since those are
posted with no window at all.

Slint looks a string up by its text *and* its context, which is the name of the component (or
global) the `@tr` is written in. Moving a string to another component is a new string.
"""

import pathlib
import re
import sys
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parents[2]
UI = ROOT / "crates" / "uplink" / "ui"
LANG = ROOT / "crates" / "uplink" / "lang"
RES = ROOT / "android" / "res"
DOMAIN = "uplink"
# `{}`, `{0}`, `{n}` in Slint's format; `%1$s`, `%d` in Java's.
SLINT_PLACEHOLDER = re.compile(r"\{[0-9n]*\}")
JAVA_PLACEHOLDER = re.compile(r"%(?:\d+\$)?[sd]")


def placeholders(pattern, text):
    return sorted(pattern.findall(text))

DECLARATION = re.compile(r"^(?:export\s+)?(?:component|global)\s+([A-Za-z_][\w-]*)", re.M)
STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')


def unescape(text):
    return re.sub(r"\\(.)", r"\1", text)


def po_quote(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def strings_at(source, index):
    """The string literal starting at `index` (after whitespace), and where it ends."""
    match = STRING.match(source, index + len(source[index:]) - len(source[index:].lstrip()))
    return (unescape(match.group(1)), match.end()) if match else (None, index)


def extract():
    """Every (context, msgid, plural) in the markup, with where each was found."""
    found = {}
    for path in sorted(UI.glob("*.slint")):
        source = path.read_text()
        declarations = [(m.start(), m.group(1)) for m in DECLARATION.finditer(source)]
        for call in re.finditer(r"@tr\(", source):
            context = next((name for start, name in reversed(declarations) if start < call.start()), "")
            first, after = strings_at(source, call.end())
            rest = source[after:].lstrip()
            if rest.startswith("=>"):
                context = first
                first, after = strings_at(source, after + len(source[after:]) - len(rest) + 2)
                rest = source[after:].lstrip()
            plural = ""
            if rest.startswith("|"):
                plural, after = strings_at(source, after + len(source[after:]) - len(rest) + 1)
            line = source.count("\n", 0, call.start()) + 1
            found.setdefault((context, first, plural), []).append(f"{path.relative_to(ROOT)}:{line}")
    return found


def parse_po(path):
    """Entries of a .po file as {(context, msgid, plural): [msgstr, ...]}."""
    entries, current, key = {}, {}, None

    def flush():
        if "msgid" in current and current["msgid"]:
            strs = [current[k] for k in sorted(k for k in current if k.startswith("msgstr"))]
            entries[(current.get("msgctxt", ""), current["msgid"], current.get("msgid_plural", ""))] = strs

    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            if not line and current:
                flush()
                current, key = {}, None
            continue
        if line.startswith('"'):
            current[key] += unescape(line[1:-1])
            continue
        key, _, value = line.partition(" ")
        if key == "msgctxt" and "msgid" in current:
            flush()
            current = {}
        current[key] = unescape(value.strip()[1:-1])
    flush()
    return entries


def pot(found):
    out = ['msgid ""', 'msgstr ""', '"Content-Type: text/plain; charset=UTF-8\\n"', ""]
    for (context, msgid, plural), places in sorted(found.items()):
        out += [f"#: {place}" for place in places]
        out.append(f"msgctxt {po_quote(context)}")
        out.append(f"msgid {po_quote(msgid)}")
        if plural:
            out += [f"msgid_plural {po_quote(plural)}", 'msgstr[0] ""', 'msgstr[1] ""']
        else:
            out.append('msgstr ""')
        out.append("")
    target = LANG / f"{DOMAIN}.pot"
    target.write_text("\n".join(out))
    print(f"{target.relative_to(ROOT)}: {len(found)} strings")


def check(found):
    problems = 0
    for po in sorted(LANG.glob(f"*/LC_MESSAGES/{DOMAIN}.po")):
        language = po.parts[-3]
        entries = parse_po(po)
        for key in sorted(found):
            if key not in entries:
                print(f"{language}: missing  {key[0]}: {key[1]!r}  ({found[key][0]})")
                problems += 1
            elif not all(entries[key]):
                print(f"{language}: empty    {key[0]}: {key[1]!r}  ({found[key][0]})")
                problems += 1
            else:
                # A plural's singular may drop `{n}` ("Missed call"), so it is held to the plural.
                source = key[2] or key[1]
                for text in entries[key]:
                    if not key[2] and placeholders(SLINT_PLACEHOLDER, text) != placeholders(SLINT_PLACEHOLDER, source):
                        print(f"{language}: holes    {key[0]}: {key[1]!r} -> {text!r}")
                        problems += 1
        for key in sorted(set(entries) - set(found)):
            print(f"{language}: obsolete {key[0]}: {key[1]!r}")
            problems += 1
        print(f"{language}: {len(entries)} entries, {len(found)} in the markup")
    return problems + check_android()


def android_texts(path):
    """A values file's strings and plurals, as {(kind, name): [text, ...]}. A plural's texts are
    its quantities' in order, with `other` first."""
    texts = {}
    for node in ET.parse(path).getroot():
        name = node.get("name")
        if node.tag == "string":
            texts[("string", name)] = ["".join(node.itertext())]
        elif node.tag == "plurals":
            items = {item.get("quantity"): "".join(item.itertext()) for item in node}
            texts[("plurals", name)] = [items.get("other", "")] + [v for k, v in items.items() if k != "other"]
    return texts


def check_android():
    problems = 0
    defaults = android_texts(RES / "values" / "values.xml")
    for folder in sorted(RES.glob("values-*")):
        language = folder.name.removeprefix("values-")
        texts = android_texts(folder / "values.xml")
        for key in sorted(defaults):
            if key not in texts:
                print(f"{language} (android): missing  {key[0]} {key[1]}")
                problems += 1
                continue
            if not all(texts[key]):
                print(f"{language} (android): empty    {key[0]} {key[1]}")
                problems += 1
            # Every quantity is held to `other`, the one that carries the count: a language may
            # spell "one" or "two" out in words.
            wanted = placeholders(JAVA_PLACEHOLDER, defaults[key][0])
            others = texts[key][:1] if key[0] == "plurals" else texts[key]
            for text in others:
                if placeholders(JAVA_PLACEHOLDER, text) != wanted:
                    print(f"{language} (android): holes    {key[1]}: {text!r}")
                    problems += 1
        for key in sorted(set(texts) - set(defaults)):
            print(f"{language} (android): obsolete {key[0]} {key[1]}")
            problems += 1
        print(f"{language} (android): {len(texts)} of {len(defaults)}")
    return problems


def main():
    found = extract()
    match sys.argv[1:]:
        case ["pot"]:
            pot(found)
        case ["check"]:
            sys.exit(1 if check(found) else 0)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main()

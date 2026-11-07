#!/usr/bin/env python3
"""Keeps en/LC_MESSAGES/duoray-gui.po in step with the code.

Collects the strings to translate: @tr("...") in ui/*.slint, and tr("..."),
trf!("..."), n("...") and plural(n, "...", "...", "...") in src/*.rs, here
and in duoray-core (tr, core_trf!, n). Translations
already in the .po are kept, strings gone from the code are dropped, new ones
come in untranslated.

    update.py                 rewrite the .po, print the untranslated count
    update.py --missing       print untranslated strings as JSON {msgid: ""}
    update.py --apply F.json  take translations from F.json {msgid: msgstr}
"""
import glob, json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.dirname(HERE)
PO = os.path.join(HERE, "en", "LC_MESSAGES", "duoray-gui.po")
LIT = r'"((?:[^"\\\n]|\\.)*)"'


def unescape(s):
    return re.sub(r'\\(.)', lambda m: {"n": "\n", "t": "\t"}.get(m.group(1), m.group(1)), s)


def escape(s):
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t")


def collect():
    found = []
    for path in sorted(glob.glob(os.path.join(CRATE, "ui", "*.slint"))):
        text = open(path, encoding="utf-8").read()
        found += [unescape(m) for m in re.findall(r"@tr\(\s*" + LIT, text)]
    core = os.path.join(os.path.dirname(CRATE), "duoray-core", "src", "*.rs")
    for path in sorted(glob.glob(os.path.join(CRATE, "src", "*.rs"))) + sorted(glob.glob(core)):
        # A backslash at a line end continues the string without the break
        # and the next line's indent.
        text = re.sub(r"\\\n\s*", "", open(path, encoding="utf-8").read())
        found += [unescape(m) for m in re.findall(r"\btr\(\s*" + LIT, text)]
        found += [unescape(m) for m in re.findall(r"\btrf!\(\s*" + LIT, text)]
        found += [unescape(m) for m in re.findall(r"core_trf!\(\s*" + LIT, text)]
        found += [unescape(m) for m in re.findall(r"\bn\(\s*" + LIT, text)]
        for args in re.findall(r"\bplural\(\s*[^,]+,\s*" + LIT + r"\s*,\s*" + LIT + r"\s*,\s*" + LIT, text):
            found += [unescape(args[0]), unescape(args[2])]
    seen, ordered = set(), []
    for s in found:
        if s and s not in seen:
            seen.add(s)
            ordered.append(s)
    return ordered


def read_po():
    if not os.path.exists(PO):
        return {}
    entries, key, field = {}, None, None
    for line in open(PO, encoding="utf-8"):
        line = line.strip()
        m = re.match(r'(msgid|msgstr) "(.*)"$', line)
        if m:
            field = m.group(1)
            if field == "msgid":
                key = unescape(m.group(2))
                entries[key] = ""
            else:
                entries[key] += unescape(m.group(2))
        elif line.startswith('"') and field:
            part = unescape(line[1:-1])
            if field == "msgid":
                value = entries.pop(key)
                key += part
                entries[key] = value
            else:
                entries[key] += part
    entries.pop("", None)
    return entries


def write_po(strings, known):
    os.makedirs(os.path.dirname(PO), exist_ok=True)
    with open(PO, "w", encoding="utf-8") as f:
        f.write('msgid ""\nmsgstr ""\n"Content-Type: text/plain; charset=UTF-8\\n"\n"Language: en\\n"\n\n')
        for s in strings:
            f.write(f'msgid "{escape(s)}"\nmsgstr "{escape(known.get(s, ""))}"\n\n')


def main():
    strings = collect()
    known = read_po()
    if len(sys.argv) > 2 and sys.argv[1] == "--apply":
        known.update({k: v for k, v in json.load(open(sys.argv[2], encoding="utf-8")).items() if v})
    if len(sys.argv) > 1 and sys.argv[1] == "--missing":
        print(json.dumps({s: "" for s in strings if not known.get(s)}, ensure_ascii=False, indent=0))
        return
    write_po(strings, known)
    missing = sum(1 for s in strings if not known.get(s))
    print(f"{len(strings)} strings, {missing} untranslated")


main()

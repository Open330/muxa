#!/usr/bin/env python3
"""Merge an Xcode localization export into Resources/Localizable.xcstrings.

Typical flow (run from apps/muxa-macos; the build must use
SWIFT_EMIT_LOC_STRINGS=YES, which project.yml sets for the Muxa target):

    MUXA_SKIP_EMBED=1 xcodebuild -exportLocalizations \\
        -project Muxa.xcodeproj -localizationPath .build/l10n \\
        -exportLanguage ko -derivedDataPath .build/DerivedData
    Scripts/l10n-sync.py --xliff .build/l10n/ko.xcloc            # add new keys
    Scripts/l10n-sync.py --localizations ko translations.json    # fill them in
    Scripts/l10n-sync.py --check                                 # 100% ko?

What it does:
  * every key the export contains is guaranteed to exist in the catalog;
  * existing translations are never touched unless --localizations names
    the key, and a changed translation is marked "needs_review" (use
    --approve to mark every translation "translated" once reviewed);
  * keys that the export no longer contains are reported (removed with
    --prune);
  * --check exits 1 while any key lacks a non-empty value for --language.

--localizations takes a JSON object of key -> value, where value is either
a plain string (becomes a stringUnit) or a full localization object (e.g. a
"variations"/"substitutions" block for plural forms) used verbatim.
"""

import argparse
import json
import os
import sys
import xml.etree.ElementTree as ET

XLIFF_NS = {"x": "urn:oasis:names:tc:xliff:document:1.2"}
HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_CATALOG = os.path.join(HERE, "..", "Resources", "Localizable.xcstrings")


def load_catalog(path):
    if not os.path.exists(path):
        return {"sourceLanguage": "en", "strings": {}, "version": "1.0"}
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def save_catalog(path, catalog):
    catalog["strings"] = dict(sorted(catalog["strings"].items()))
    text = json.dumps(catalog, ensure_ascii=False, indent=2, separators=(",", " : "))
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text + "\n")


def xliff_files(path):
    """Yield every .xliff under an .xcloc directory (or the file itself)."""
    if os.path.isdir(path):
        for root, _, files in os.walk(path):
            for name in sorted(files):
                if name.endswith(".xliff"):
                    yield os.path.join(root, name)
    else:
        yield path


def exported_keys(xliff_path, catalog_name):
    """Keys (with comments) the export attributes to `catalog_name`.

    Plural/device variations are exported as `<key>|==|plural.one`; they all
    belong to the base key. Files for other catalogs (InfoPlist, other
    targets) are ignored so this stays scoped to one catalog.
    """
    keys = {}
    tree = ET.parse(xliff_path)
    for file_node in tree.getroot().findall("x:file", XLIFF_NS):
        original = file_node.get("original", "")
        if os.path.basename(original) != catalog_name:
            continue
        for unit in file_node.iter("{%s}trans-unit" % XLIFF_NS["x"]):
            unit_id = unit.get("id") or ""
            key = unit_id.split("|==|", 1)[0]
            if not key:
                continue
            note = unit.find("x:note", XLIFF_NS)
            comment = (note.text or "").strip() if note is not None else ""
            keys.setdefault(key, comment)
    return keys


def localization_has_value(localization):
    if not isinstance(localization, dict):
        return False
    unit = localization.get("stringUnit")
    if isinstance(unit, dict) and unit.get("value"):
        return True
    variations = localization.get("variations", {})
    for forms in variations.values():
        if isinstance(forms, dict) and forms and all(
            localization_has_value(form) for form in forms.values()
        ):
            return True
    substitutions = localization.get("substitutions")
    if isinstance(substitutions, dict) and substitutions and unit and unit.get("value"):
        return True
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--catalog", default=DEFAULT_CATALOG, help="path to the .xcstrings file")
    parser.add_argument("--language", default="ko", help="target language to fill/check (default: ko)")
    parser.add_argument("--xliff", help="exported .xcloc directory or .xliff file to merge keys from")
    parser.add_argument(
        "--localizations",
        nargs=2,
        metavar=("LANG", "JSON"),
        action="append",
        default=[],
        help="apply key -> value/object translations for LANG from a JSON file",
    )
    parser.add_argument("--prune", action="store_true", help="remove keys missing from the export")
    parser.add_argument("--approve", action="store_true", help="mark every --language value as translated")
    parser.add_argument("--check", action="store_true", help="exit 1 while any key lacks a --language value")
    args = parser.parse_args()

    catalog = load_catalog(args.catalog)
    strings = catalog.setdefault("strings", {})
    changed = False

    if args.xliff:
        exported = {}
        for xliff in xliff_files(args.xliff):
            exported.update(exported_keys(xliff, os.path.basename(args.catalog)))
        if not exported:
            sys.exit(f"no keys for {os.path.basename(args.catalog)} found in {args.xliff}")
        added = 0
        for key, comment in exported.items():
            entry = strings.setdefault(key, {})
            if "localizations" not in entry:
                entry["localizations"] = {}
            if comment and not entry.get("comment"):
                entry["comment"] = comment
            if key not in strings or not entry.get("localizations", {}).get(args.language):
                if entry.get("extractionState") != "manual":
                    added += 1
            entry.setdefault("localizations", {})
        for key in list(strings):
            if key not in exported:
                if args.prune:
                    del strings[key]
                    changed = True
                    print(f"pruned stale key: {key!r}")
                else:
                    print(f"stale key (not in export): {key!r}", file=sys.stderr)
        stale = [key for key in strings if key not in exported]
        print(f"export: {len(exported)} keys; catalog now {len(strings)} keys; {len(stale)} stale")
        changed = True

    for language, json_path in args.localizations:
        with open(json_path, encoding="utf-8") as handle:
            translations = json.load(handle)
        applied = 0
        for key, value in translations.items():
            entry = strings.setdefault(key, {"localizations": {}})
            localizations = entry.setdefault("localizations", {})
            if isinstance(value, str):
                current = localizations.get(language, {}).get("stringUnit", {}).get("value")
                state = "translated" if current == value else "needs_review"
                new = {"stringUnit": {"state": state, "value": value}}
            else:
                new = value
            if localizations.get(language) != new:
                localizations[language] = new
                applied += 1
        print(f"{language}: applied {applied} of {len(translations)} translations")
        changed = changed or applied > 0

    if args.approve:
        approved = 0
        for entry in strings.values():
            localization = entry.get("localizations", {}).get(args.language)
            unit = localization.get("stringUnit") if isinstance(localization, dict) else None
            if isinstance(unit, dict) and unit.get("value") and unit.get("state") != "translated":
                unit["state"] = "translated"
                approved += 1
        print(f"approved {approved} {args.language} translations")
        changed = changed or approved > 0

    if changed:
        save_catalog(args.catalog, catalog)
        print(f"wrote {args.catalog}")

    missing = [
        key
        for key, entry in strings.items()
        if not localization_has_value(entry.get("localizations", {}).get(args.language))
    ]
    if missing:
        print(f"{len(missing)} keys without a {args.language} value:")
        for key in sorted(missing):
            print(f"  {key!r}")
    else:
        print(f"every key ({len(strings)}) has a {args.language} value")
    if args.check and missing:
        sys.exit(1)


if __name__ == "__main__":
    main()

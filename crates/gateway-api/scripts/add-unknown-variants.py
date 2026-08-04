#!/usr/bin/env python3
"""Add an `Unknown` catch-all to every enum kopium generates from a CRD enum.

kopium turns a CRD's `enum: [...]` into a closed Rust enum. Kubernetes API
enums are not closed: every Gateway API release may add a member (v1.6.1 added
`CORS` to HTTPRoute's filter types). A closed enum makes the *whole list page*
fail to deserialize, so one object carrying a newer member hides every object
of that kind from the controller — the store never syncs and the process exits.

`#[serde(other)]` gives the enum a landing place for anything it does not know,
which the builder then reports as an unsupported feature instead of crashing.

Run from crates/gateway-api/ as part of the regeneration documented in
README.md. Idempotent: an enum that already has the variant is left alone.
"""

import pathlib
import re
import sys

VARIANT = "    /// A member this build does not know. Kubernetes API enums are open:\n" \
          "    /// a newer CRD may carry values generated before they existed, and a\n" \
          "    /// closed enum would fail the whole list page rather than one object.\n" \
          "    #[serde(other)]\n" \
          "    Unknown,\n"

# `#[serde(other)]` needs a string tag. Enums whose members are numbers (kopium
# renames them to "301"/"302") deserialize from JSON strings here too — the CRD
# declares them as strings — so they take the variant like the rest.
ENUM_RE = re.compile(r"(pub enum (\w+) \{\n)((?:.*?\n)*?)(\})", re.M)


def patch(path: pathlib.Path) -> int:
    src = path.read_text()
    count = 0

    def repl(m: re.Match) -> str:
        nonlocal count
        head, name, body, tail = m.groups()
        if "Unknown," in body:
            return m.group(0)
        count += 1
        return f"{head}{body}{VARIANT}{tail}"

    out = ENUM_RE.sub(repl, src)
    if count:
        path.write_text(out)
    return count


def main() -> int:
    src_dir = pathlib.Path(__file__).resolve().parent.parent / "src"
    if not src_dir.is_dir():
        print(f"not found: {src_dir}", file=sys.stderr)
        return 1
    total = 0
    for f in sorted(src_dir.glob("*.rs")):
        n = patch(f)
        if n:
            print(f"  {f.name}: {n} enum(s) patched")
        total += n
    print(f"{total} enum(s) given an Unknown variant")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

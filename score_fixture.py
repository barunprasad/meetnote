#!/usr/bin/env python3
"""score_fixture.py — score transcripts of the fixture against known ground truth.

    ./score_fixture.py fixtures/demo/transcript.*.txt

The fixture dialogue is fixed and known, so engine comparison can be a number
rather than an impression. Tokens are grouped by what they actually test,
because the groups matter differently: a missed discourse marker costs the note
nothing, while a missed name or figure is a hole in the record.

Stdlib only.
"""

import argparse
import sys
from pathlib import Path

# Grouped by consequence, not by part of speech.
CHECKS = {
    "names": [("Priya", ["priya"]), ("Rohan", ["rohan"]), ("Meera", ["meera", "mira"])],
    "numbers": [
        ("40%", ["40%", "forty percent"]),
        ("12,000", ["12,000", "12000", "twelve thousand"]),
        ("v2.4", ["2.4", "two point four"]),
        ("3 weeks", ["3 weeks", "three weeks"]),
    ],
    "jargon": [
        ("Postgres", ["postgres"]), ("idempotency", ["idempotenc"]), ("webhook", ["webhook"]),
        ("Kafka", ["kafka"]), ("on-call", ["on-call", "on call"]), ("escalation", ["escalation"]),
        ("tribal knowledge", ["tribal knowledge"]),
    ],
    # The known weak spot. These carry almost no meaning for a note, which is
    # why failing them is survivable in a way that failing the groups above is not.
    "hinglish": [("Haan", ["haan"]), ("Theek hai", ["theek hai"]), ("Thoda", ["thoda"]),
                 ("Matlab", ["matlab"])],
    "content": [("Rohan nights", ["covering nights"]), ("dashboard", ["dashboard"]),
                ("Tuesdays", ["tuesdays"]), ("staging", ["staging"])],
}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("transcripts", nargs="+", type=Path)
    args = parser.parse_args()

    texts = {}
    for path in args.transcripts:
        if not path.exists():
            continue
        # transcript.whisper-large-v3-8bit.txt -> whisper-large-v3-8bit
        label = path.stem.replace("transcript.", "") or path.stem
        texts[label] = path.read_text().lower()
    if not texts:
        sys.exit("no transcripts found")

    width = max(len(l) for l in texts) + 2
    header = "".join(l.rjust(width) for l in texts)
    print(f"{'':22}{header}")

    totals = {label: 0 for label in texts}
    group_totals = {label: {} for label in texts}
    count = 0
    for group, items in CHECKS.items():
        print(f"\n  {group}")
        for label, variants in items:
            count += 1
            row = ""
            for engine, text in texts.items():
                hit = any(v in text for v in variants)
                totals[engine] += hit
                group_totals[engine][group] = group_totals[engine].get(group, 0) + hit
                row += ("✓" if hit else "·").rjust(width)
            print(f"    {label:<20}{row}")

    print(f"\n  {'':20}" + "".join(("─" * (width - 2)).rjust(width) for _ in texts))
    for group in CHECKS:
        row = "".join(f"{group_totals[e].get(group, 0)}/{len(CHECKS[group])}".rjust(width) for e in texts)
        print(f"  {group:<20}{row}")
    print(f"  {'TOTAL':<20}" + "".join(f"{totals[e]}/{count}".rjust(width) for e in texts))


if __name__ == "__main__":
    main()

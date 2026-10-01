"""Write a Markdown summary of an interop known-gaps run for the GitHub job summary.

Usage: summarize.py <junit-xml> <runtime>

Reads the JUnit report written by `cargo nextest run --profile interop
--run-ignored only` and prints a table of the ignored tests and whether each
one still fails. A test that now passes is listed first, so its `#[ignore]`
can be removed. The script always exits 0: when the report is missing or
unreadable it says so in the summary, and the step that failed to write it
carries the failure.
"""

import sys
import xml.etree.ElementTree as ET
from pathlib import Path


def results(root: ET.Element) -> list[tuple[str, bool]]:
    """Return (test name, passed) for every test that ran, sorted by name."""
    rows = []
    for case in root.iter("testcase"):
        if case.find("skipped") is not None:
            continue
        failed = case.find("failure") is not None or case.find("error") is not None
        rows.append((case.get("name", "(unnamed)"), not failed))
    return sorted(rows)


def summarize(path: Path, runtime: str) -> str:
    lines = [f"### Interop known gaps: {runtime}", ""]
    try:
        root = ET.parse(path).getroot()
    except FileNotFoundError:
        lines.append(
            f"No test report was found at `{path}`. The test step stopped "
            "before writing it; see that step's log."
        )
        return "\n".join(lines)
    except ET.ParseError as err:
        lines.append(
            f"The test report at `{path}` could not be read ({err}). "
            "See the test step's log."
        )
        return "\n".join(lines)

    rows = results(root)
    if not rows:
        lines.append("No ignored interop tests ran for this runtime.")
        return "\n".join(lines)

    now_passing = [name for name, passed in rows if passed]
    if now_passing:
        lines += ["These ignored tests now pass; remove their `#[ignore]`:", ""]
        lines += [f"- `{name}`" for name in now_passing]
        lines.append("")

    lines += ["| Test | Result |", "|---|---|"]
    lines += [
        f"| `{name}` | {'**passes**' if passed else 'fails'} |" for name, passed in rows
    ]
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"usage: {argv[0]} <junit-xml> <runtime>", file=sys.stderr)
        return 2
    print(summarize(Path(argv[1]), argv[2]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

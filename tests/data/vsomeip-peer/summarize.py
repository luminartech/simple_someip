"""Report on an interop known-gaps run in GitHub Actions.

Usage:
    summarize.py <junit-xml> <runtime>                 Markdown job summary
    summarize.py --annotations <junit-xml> <runtime>   one warning per fixed gap

Reads the JUnit report written by `cargo nextest run --profile interop
--run-ignored only`. By default it prints a table of the ignored tests and
whether each one still fails, listing first any that now pass, so their
`#[ignore]` can be removed. With `--annotations` it prints a `::warning::`
workflow command for each test that now passes instead, which GitHub shows on
the checks page.

The script always exits 0 for a report it cannot read: the summary says so,
no annotation is printed, and the step that failed to write it carries the
failure.
"""

import argparse
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


def load(path: Path) -> list[tuple[str, bool]] | str:
    """Return the results in the report, or why it could not be read."""
    try:
        return results(ET.parse(path).getroot())
    except FileNotFoundError:
        return (
            f"No test report was found at `{path}`. The test step stopped "
            "before writing it; see that step's log."
        )
    except ET.ParseError as err:
        return (
            f"The test report at `{path}` could not be read ({err}). "
            "See the test step's log."
        )


def summarize(path: Path, runtime: str) -> str:
    lines = [f"### Interop known gaps: {runtime}", ""]
    rows = load(path)
    if isinstance(rows, str):
        lines.append(rows)
        return "\n".join(lines)
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


def escape(value: str) -> str:
    """Escape text for the message part of a GitHub workflow command."""
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def annotations(path: Path, runtime: str) -> str:
    rows = load(path)
    if isinstance(rows, str):
        return ""
    return "\n".join(
        f"::warning title=Known interop gap now passes::"
        f"{escape(f'{name} ({runtime}) now passes; remove its #[ignore].')}"
        for name, passed in rows
        if passed
    )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--annotations",
        action="store_true",
        help="print a ::warning:: for each ignored test that now passes",
    )
    parser.add_argument("junit", type=Path, help="nextest JUnit report")
    parser.add_argument("runtime", help="runtime name shown in the output")
    args = parser.parse_args(argv)
    out = (annotations if args.annotations else summarize)(args.junit, args.runtime)
    if out:
        print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

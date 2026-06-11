#!/usr/bin/env python3

import argparse


def main() -> None:
    parser = argparse.ArgumentParser(description="Standalone placeholder Apple installer.")
    parser.parse_known_args()
    raise SystemExit(
        "Apple install support is not configured in this standalone prelude. "
        "Pass an explicit installer target to use a project-specific installer."
    )


if __name__ == "__main__":
    main()

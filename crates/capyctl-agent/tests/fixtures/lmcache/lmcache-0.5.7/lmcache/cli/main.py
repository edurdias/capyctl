# Excerpt of LMCache v0.5.7 lmcache/cli/main.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/cli/main.py

def main(argv: Optional[Sequence[str]] = None) -> None:
    """CLI entry point registered as ``lmcache`` in *pyproject.toml*.

    Args:
        argv: Invocation tokens after the program name.  ``None`` (the
            console-script path) reads ``sys.argv[1:]``.
    """
    tokens = list(sys.argv[1:] if argv is None else argv)

    print_banner_once(sys.stderr)
    parser = argparse.ArgumentParser(
        prog="lmcache",
        description="LMCache — KV cache management for LLM serving",
    )
    subparsers = parser.add_subparsers(dest="command")

    register_commands(subparsers, tokens)

    args = parser.parse_args(tokens)

    if not hasattr(args, "func"):
        parser.print_help()
        sys.exit(1)

    try:
        args.func(args)
    except KeyboardInterrupt:
        sys.exit(130)
    except Exception:  # noqa: BLE001
        logger.exception("Command failed")
        sys.exit(1)


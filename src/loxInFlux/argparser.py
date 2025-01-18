import argparse

_parser = argparse.ArgumentParser(description="LoxInFlux")
_args = None

def get_args() -> argparse.Namespace:
    global _args, _parser
    if _args is None:
        _parser.add_argument(
        "--log-level",
        type=str,
        choices=["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"],
        help="Set the logging level (overrides config.json setting)"
        )
        _parser.add_argument(
            "--config-dir",
            type=str,
            help="Directory containing config.toml (default: config/)"
        )
        _parser.add_argument(
            "--data-dir",
            type=str,
            help="Directory for storing miniserver data (default: data/)"
        )
        _args = _parser.parse_args()
    return _args
"""Runtime build command implementation."""
import os
import sys
import argparse
from avocado.commands.base import BaseCommand


class RuntimeBuildCommand(BaseCommand):
    """Implementation of the 'runtime build' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the runtime build command's subparser."""
        parser = subparsers.add_parser(
            "build",
            help="Build a runtime"
        )

        # Optional argument - the config file path
        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        return parser

    def execute(self, args, parser=None):
        """Execute the runtime build command."""
        config_path = args.config

        if not os.path.exists(config_path):
            print(f"Error: Configuration file '{config_path}' not found.", file=sys.stderr)
            print("Run 'avocado init' first or specify a valid configuration file with --config.", file=sys.stderr)
            return False

        # For now, just print a message that the command exists but does nothing
        print(f"Runtime build command executed for project using config: {os.path.abspath(config_path)}")
        print("(This command does nothing for now)")
        return True
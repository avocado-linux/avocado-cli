"""Runtime command group implementation."""

import argparse
import sys
from avocado.commands.base import BaseCommand
from .build import RuntimeBuildCommand
from .list import RuntimeListCommand
from .deps import RuntimeDepsCommand


class RuntimeCommand(BaseCommand):
    """Implementation of the 'runtime' command group."""

    def __init__(self):
        # Store references to the subparsers if needed by execute
        self.build_parser = None
        self.list_parser = None
        self.deps_parser = None

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the runtime command's subparser."""
        runtime_parser = subparsers.add_parser(
            "runtime", help="Runtime management commands"
        )
        runtime_subparsers = runtime_parser.add_subparsers(
            dest="runtime_subcommand",
            help="Runtime subcommands",
            required=False,  # Allow no subcommand to show help
        )

        # Register Runtime subcommands
        build_parser = RuntimeBuildCommand.register_subparser(runtime_subparsers)
        runtime_parser.build_parser = build_parser

        # RuntimeListCommand
        list_parser = RuntimeListCommand.register_subparser(runtime_subparsers)
        runtime_parser.list_parser = list_parser

        # RuntimeDepsCommand
        deps_parser = RuntimeDepsCommand.register_subparser(runtime_subparsers)
        runtime_parser.deps_parser = deps_parser

        return runtime_parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the runtime command."""
        # Check if we have a subcommand
        if not hasattr(args, "runtime_subcommand") or args.runtime_subcommand is None:
            if parser:
                parser.print_help()
                return True  # Return success when showing help
            else:
                print(
                    "Available runtime subcommands: 'build', 'list', 'deps'",
                    file=sys.stderr,
                )
            return False

        # Dispatch to appropriate runtime subcommand
        if args.runtime_subcommand == "build":
            command = RuntimeBuildCommand()
            sub_parser = getattr(parser, "build_parser", None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.runtime_subcommand == "list":
            command = RuntimeListCommand()
            sub_parser = getattr(parser, "list_parser", None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.runtime_subcommand == "deps":
            command = RuntimeDepsCommand()
            sub_parser = getattr(parser, "deps_parser", None) if parser else None
            return command.execute(args, sub_parser, unknown)
        else:
            print(
                f"Unknown runtime subcommand: {
                  args.runtime_subcommand}",
                file=sys.stderr,
            )
            if parser:
                parser.print_help()
            return False


# Make RuntimeCommand easily importable
__all__ = ["RuntimeCommand"]

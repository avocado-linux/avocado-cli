"""SDK command group implementation."""
import argparse
from avocado.commands.base import BaseCommand
from .run import SdkRunCommand


class SdkCommand(BaseCommand):
    """Implementation of the 'sdk' command group."""

    def __init__(self):
        self.run_parser = None

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk command's subparser."""
        sdk_parser = subparsers.add_parser("sdk", help="SDK management commands")
        sdk_subparsers = sdk_parser.add_subparsers(
            dest="sdk_subcommand",
            help="SDK subcommands"
        )

        # Register SDK subcommands and store parser references
        run_parser = SdkRunCommand.register_subparser(sdk_subparsers)

        # Store parser reference in the SDK parser for later access
        sdk_parser.run_parser = run_parser

        return sdk_parser

    def execute(self, args, parser=None):
        """Execute the sdk command."""
        # Debug: Print what we received
        # print(f"DEBUG: args = {args}")
        # print(f"DEBUG: hasattr sdk_subcommand = {hasattr(args, 'sdk_subcommand')}")
        # if hasattr(args, 'sdk_subcommand'):
        #     print(f"DEBUG: sdk_subcommand = {args.sdk_subcommand}")

        # Check if we have a subcommand
        if not hasattr(args, 'sdk_subcommand') or args.sdk_subcommand is None:
            if parser:
                parser.print_help()
            else:
                print("Error: SDK subcommand required")
                print("Available subcommands: run")
            return False

        # Dispatch to appropriate SDK subcommand
        if args.sdk_subcommand == "run":
            command = SdkRunCommand()
            # Pass the run parser if available
            run_parser = getattr(parser, 'run_parser', None) if parser else None
            return command.execute(args, run_parser)
        else:
            print(f"Unknown SDK subcommand: {args.sdk_subcommand}")
            return False


# SDK-wide utilities and constants
SDK_VERSION = "1.0.0"
DEFAULT_SDK_PATH = "/opt/avocado-sdk"

# Make SdkCommand easily importable
__all__ = ['SdkCommand']

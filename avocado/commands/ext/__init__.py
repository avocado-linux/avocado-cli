"""Extension command group implementation."""
import argparse
import sys
from avocado.commands.base import BaseCommand
from .install import ExtInstallCommand
from .build import ExtBuildCommand


class ExtCommand(BaseCommand):
    """Implementation of the 'ext' command group."""

    def __init__(self):
        # These can store references to the subparsers if needed by execute
        self.install_parser = None
        self.build_parser = None

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext command's subparser."""
        ext_parser = subparsers.add_parser("ext", help="Extension management commands")
        ext_subparsers = ext_parser.add_subparsers(
            dest="ext_subcommand",
            help="Extension subcommands",
            required=False  # Allow no subcommand to show help
        )

        # Register Extension subcommands
        # Storing parser references on ext_parser can be useful if execute needs them
        
        # ExtInstallCommand
        install_parser = ExtInstallCommand.register_subparser(ext_subparsers)
        ext_parser.install_parser = install_parser

        # ExtBuildCommand
        build_parser = ExtBuildCommand.register_subparser(ext_subparsers)
        ext_parser.build_parser = build_parser
        
        return ext_parser

    def execute(self, args, parser=None):
        """Execute the ext command."""
        # Check if we have a subcommand
        if not hasattr(args, 'ext_subcommand') or args.ext_subcommand is None:
            if parser:
                parser.print_help()
                return True  # Return success when showing help
            else:
                print("Available extension subcommands: 'install', 'build'", file=sys.stderr)
            return False

        # Dispatch to appropriate Extension subcommand
        if args.ext_subcommand == "install":
            command = ExtInstallCommand()
            sub_parser = getattr(parser, 'install_parser', None) if parser else None
            return command.execute(args, sub_parser)
        elif args.ext_subcommand == "build":
            command = ExtBuildCommand()
            sub_parser = getattr(parser, 'build_parser', None) if parser else None
            return command.execute(args, sub_parser)
        else:
            print(f"Unknown Extension subcommand: {args.ext_subcommand}", file=sys.stderr)
            if parser:
                parser.print_help() 
            return False


# Extension-wide utilities and constants
DEFAULT_EXT_DIR = "extensions"

# Make ExtCommand easily importable
__all__ = ['ExtCommand']
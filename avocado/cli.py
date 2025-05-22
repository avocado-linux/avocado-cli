"""Command line interface for avocado."""

import sys
import argparse
from avocado.commands.sdk import SdkCommand
from avocado.commands.ext import ExtCommand
from avocado.commands.init import InitCommand
from avocado.commands.runtime import RuntimeCommand
from avocado.commands.clean import CleanCommand


def main():
    """Main entry point for the CLI."""
    parser = argparse.ArgumentParser(
        description="Avocado CLI tool",
        formatter_class=argparse.RawDescriptionHelpFormatter
    )
    subparsers = parser.add_subparsers(
        dest="main_command", help="Command to execute")

    # Register commands and store their parsers
    sdk_parser = SdkCommand.register_subparser(subparsers)
    ext_parser = ExtCommand.register_subparser(subparsers)
    init_parser = InitCommand.register_subparser(subparsers)
    runtime_parser = RuntimeCommand.register_subparser(subparsers)
    clean_parser = CleanCommand.register_subparser(subparsers)

    args, unknown = parser.parse_known_args()

    if not args.main_command:
        parser.print_help()
        return 0

    # Dispatch to appropriate command
    if args.main_command == "sdk":
        command = SdkCommand()
        success = command.execute(args, sdk_parser, unknown)
    elif args.main_command == "ext":
        command = ExtCommand()
        success = command.execute(args, ext_parser, unknown)
    elif args.main_command == "init":
        command = InitCommand()
        success = command.execute(args, init_parser, unknown)
    elif args.main_command == "runtime":
        command = RuntimeCommand()
        success = command.execute(args, runtime_parser, unknown)
    elif args.main_command == "clean":
        command = CleanCommand()
        success = command.execute(args, clean_parser, unknown)
    else:
        print(f"Unknown command: {args.main_command}")
        return 1

    return 0 if success else 1


if __name__ == "__main__":
    sys.exit(main())

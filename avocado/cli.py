"""Command line interface for avocado."""

import sys
import argparse
from avocado.commands.sdk import SdkCommand
from avocado.commands.ext import ExtCommand
from avocado.commands.init import InitCommand
from avocado.commands.runtime import RuntimeCommand
from avocado.commands.clean import CleanCommand
from avocado.utils.target import resolve_target


def main():
    """Main entry point for the CLI."""
    parser = argparse.ArgumentParser(
        description="Avocado CLI tool",
        formatter_class=argparse.RawDescriptionHelpFormatter
    )

    # Add global target argument
    parser.add_argument(
        "--target", "-t",
        help="Target architecture/board (can also be set via AVOCADO_TARGET env var)"
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

    # Resolve target from CLI args, environment, and config (future)
    resolved_target = resolve_target(cli_target=args.target)

    # Store the resolved target back in args for commands to use
    args.resolved_target = resolved_target

    if not args.main_command:
        parser.print_help()
        return 0

    # Resolve target with proper precedence (CLI > env var > config)
    resolved_target = resolve_target(cli_target=args.target)
    # Store resolved target back in args for commands to use
    args.resolved_target = resolved_target

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

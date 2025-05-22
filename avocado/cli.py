"""Command line interface for avocado."""

import sys
import argparse
from avocado.commands.build import BuildCommand
from avocado.commands.provision import ProvisionCommand
from avocado.commands.sdk import SdkCommand

def main():
    """Main entry point for the CLI."""
    parser = argparse.ArgumentParser(
        description="Avocado CLI tool",
        formatter_class=argparse.RawDescriptionHelpFormatter
    )
    subparsers = parser.add_subparsers(dest="main_command", help="Command to execute")

    # Register commands and store their parsers
    build_parser = BuildCommand.register_subparser(subparsers)
    provision_parser = ProvisionCommand.register_subparser(subparsers)
    sdk_parser = SdkCommand.register_subparser(subparsers)

    args = parser.parse_args()
    # print(f"MAIN CLI DEBUG: args.main_command = {getattr(args, 'main_command', 'NOT SET')}")
    # print(f"MAIN CLI DEBUG: full args = {args}")

    if not args.main_command:
        parser.print_help()
        return 1

    # Dispatch to appropriate command
    if args.main_command == "build":
        command = BuildCommand()
        success = command.execute(args, build_parser)
    elif args.main_command == "provision":
        command = ProvisionCommand()
        success = command.execute(args, provision_parser)
    elif args.main_command == "sdk":
        command = SdkCommand()
        success = command.execute(args, sdk_parser)
    else:
        print(f"Unknown command: {args.main_command}")
        return 1

    return 0 if success else 1

if __name__ == "__main__":
    sys.exit(main())

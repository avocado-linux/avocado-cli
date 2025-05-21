"""Command line interface for avocado."""

import sys
import argparse
from avocado.commands import BuildCommand, ProvisionCommand

def main():
    """Main entry point for the CLI."""
    parser = argparse.ArgumentParser(
        description="Avocado CLI tool",
        formatter_class=argparse.RawDescriptionHelpFormatter
    )
    subparsers = parser.add_subparsers(dest="command", help="Command to execute")

    # Register commands
    BuildCommand.register_subparser(subparsers)
    ProvisionCommand.register_subparser(subparsers)

    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        return 1

    # Dispatch to appropriate command
    if args.command == "build":
        command = BuildCommand()
        success = command.execute(args)
    elif args.command == "provision":
        command = ProvisionCommand()
        success = command.execute(args)
    else:
        print(f"Unknown command: {args.command}")
        return 1

    return 0 if success else 1


if __name__ == "__main__":
    sys.exit(main())

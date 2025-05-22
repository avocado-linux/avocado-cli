"""SDK command group implementation."""
import sys
from avocado.commands.base import BaseCommand
from .run import SdkRunCommand
from .deps import SdkDepsCommand
from .compile import SdkCompileCommand  # Added compile command
from .dnf import SdkDnfCommand
from .install import SdkInstallCommand


class SdkCommand(BaseCommand):
    """Implementation of the 'sdk' command group."""

    def __init__(self):
        # These can store references to the subparsers if needed by execute
        self.run_parser = None
        self.deps_parser = None
        self.compile_parser = None
        self.dnf_parser = None
        self.install_parser = None

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk command's subparser."""
        sdk_parser = subparsers.add_parser(
            "sdk", help="SDK management commands")
        sdk_subparsers = sdk_parser.add_subparsers(
            dest="sdk_subcommand",
            help="SDK subcommands",
            required=False  # Allow no subcommand to show help
        )

        # Register SDK subcommands
        # Storing parser references on sdk_parser can be useful if execute needs them

        # SdkRunCommand
        run_parser = SdkRunCommand.register_subparser(sdk_subparsers)
        sdk_parser.run_parser = run_parser

        # SdkDepsCommand
        deps_parser = SdkDepsCommand.register_subparser(sdk_subparsers)
        sdk_parser.deps_parser = deps_parser

        # SdkCompileCommand
        compile_parser = SdkCompileCommand.register_subparser(sdk_subparsers)
        sdk_parser.compile_parser = compile_parser

        # SdkDnfCommand
        dnf_parser = SdkDnfCommand.register_subparser(sdk_subparsers)
        sdk_parser.dnf_parser = dnf_parser

        # SdkInstallCommand
        install_parser = SdkInstallCommand.register_subparser(sdk_subparsers)
        sdk_parser.install_parser = install_parser

        return sdk_parser

    def execute(self, args, parser=None, unknown=None):
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
                return True  # Return success when showing help
            else:
                print(
                    "Available SDK subcommands: 'run', 'deps', 'compile', 'dnf', 'install'", file=sys.stderr)
            return False

        # Dispatch to appropriate SDK subcommand
        if args.sdk_subcommand == "run":
            command = SdkRunCommand()
            # Pass the specific subparser if the command's execute method needs it
            sub_parser = getattr(parser, 'run_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.sdk_subcommand == "deps":
            command = SdkDepsCommand()
            sub_parser = getattr(parser, 'deps_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.sdk_subcommand == "compile":
            command = SdkCompileCommand()
            sub_parser = getattr(parser, 'compile_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.sdk_subcommand == "dnf":
            command = SdkDnfCommand()
            sub_parser = getattr(parser, 'dnf_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.sdk_subcommand == "install":
            command = SdkInstallCommand()
            sub_parser = getattr(parser, 'install_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        else:
            # This path should ideally not be hit if 'required=True' and subcommands are correctly registered.
            print(f"Unknown SDK subcommand: {
                  args.sdk_subcommand}", file=sys.stderr)
            if parser:
                parser.print_help()
            return False


# SDK-wide utilities and constants
SDK_VERSION = "1.0.0"
DEFAULT_SDK_PATH = "/opt/avocado-sdk"

# Make SdkCommand easily importable
__all__ = ['SdkCommand']

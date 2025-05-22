"""Extension command group implementation."""
import sys
from avocado.commands.base import BaseCommand
from .build import ExtBuildCommand
from .install import ExtInstallCommand
from .list import ExtListCommand
from .deps import ExtDepsCommand
from .dnf import ExtDnfCommand
from .clean import ExtCleanCommand
from .image import ExtImageCommand


class ExtCommand(BaseCommand):
    """Implementation of the 'ext' command group."""

    def __init__(self):
        # These can store references to the subparsers if needed by execute
        self.build_parser = None
        self.install_parser = None
        self.list_parser = None
        self.deps_parser = None
        self.dnf_parser = None
        self.clean_parser = None
        self.image_parser = None

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext command's subparser."""
        ext_parser = subparsers.add_parser(
            "ext", help="Extension management commands")
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

        # ExtListCommand
        list_parser = ExtListCommand.register_subparser(ext_subparsers)
        ext_parser.list_parser = list_parser

        # ExtDepsCommand
        deps_parser = ExtDepsCommand.register_subparser(ext_subparsers)
        ext_parser.deps_parser = deps_parser

        # ExtDnfCommand
        dnf_parser = ExtDnfCommand.register_subparser(ext_subparsers)
        ext_parser.dnf_parser = dnf_parser

        # ExtCleanCommand
        clean_parser = ExtCleanCommand.register_subparser(ext_subparsers)
        ext_parser.clean_parser = clean_parser

        # ExtImageCommand
        image_parser = ExtImageCommand.register_subparser(ext_subparsers)
        ext_parser.image_parser = image_parser

        return ext_parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext command."""
        # Check if we have a subcommand
        if not hasattr(args, 'ext_subcommand') or args.ext_subcommand is None:
            if parser:
                parser.print_help()
                return True  # Return success when showing help
            else:
                print(
                    "Available extension subcommands: 'install', 'build', 'list', 'deps', 'dnf', 'clean', 'image'", file=sys.stderr)
            return False

        # Dispatch to appropriate Extension subcommand
        if args.ext_subcommand == "install":
            command = ExtInstallCommand()
            sub_parser = getattr(parser, 'install_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.ext_subcommand == "build":
            command = ExtBuildCommand()
            sub_parser = getattr(parser, 'build_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.ext_subcommand == "list":
            command = ExtListCommand()
            sub_parser = getattr(parser, 'list_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.ext_subcommand == "deps":
            command = ExtDepsCommand()
            sub_parser = getattr(parser, 'deps_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.ext_subcommand == "dnf":
            command = ExtDnfCommand()
            sub_parser = getattr(parser, 'dnf_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.ext_subcommand == "clean":
            command = ExtCleanCommand()
            sub_parser = getattr(parser, 'clean_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        elif args.ext_subcommand == "image":
            command = ExtImageCommand()
            sub_parser = getattr(parser, 'image_parser',
                                 None) if parser else None
            return command.execute(args, sub_parser, unknown)
        else:
            print(f"Unknown Extension subcommand: {
                  args.ext_subcommand}", file=sys.stderr)
            if parser:
                parser.print_help()
            return False


# Extension-wide utilities and constants
DEFAULT_EXT_DIR = "extensions"

# Make ExtCommand easily importable
__all__ = ['ExtCommand']

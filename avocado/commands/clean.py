"""Clean command implementation."""

import os
import shutil
from avocado.commands.base import BaseCommand
from avocado.utils.output import print_error, print_success, print_info


class CleanCommand(BaseCommand):
    """Implementation of the 'clean' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the clean command's subparser."""
        parser = subparsers.add_parser(
            "clean", help="Clean the avocado project by removing the _avocado directory"
        )

        # Optional argument - the directory to clean
        parser.add_argument(
            "directory",
            nargs="?",
            default=".",
            help="Directory to clean (defaults to current directory)",
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the clean command."""
        directory = args.directory

        # Resolve the full path to the directory
        if not os.path.isabs(directory):
            directory = os.path.abspath(directory)

        # Check if the directory exists
        if not os.path.exists(directory):
            print_error(f"Directory '{directory}' does not exist.")
            return False

        # Path to the _avocado directory
        avocado_dir = os.path.join(directory, "_avocado")

        # Check if _avocado directory exists
        if not os.path.exists(avocado_dir):
            print_info(f"No _avocado directory found in '{directory}'.")
            return True

        # Remove the _avocado directory
        try:
            shutil.rmtree(avocado_dir)
            print_success(f"Removed _avocado directory from '{directory}'.")
            return True
        except Exception as e:
            print_error(f"Failed to remove _avocado directory: {str(e)}")
            return False

# commands/provision.py

import os
from .base import BaseCommand


class ProvisionCommand(BaseCommand):
    """Implementation of the 'provision' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the provision command's subparser."""
        provision_parser = subparsers.add_parser("provision", help="Run provisioning")
        provision_parser.add_argument("--cwd", help="Working directory for provisioning (optional)")

    def execute(self, args):
        """Execute the provision command."""
        working_dir = args.cwd
        original_dir = os.getcwd()

        # Change working directory if specified
        if working_dir:
            try:
                os.chdir(working_dir)
                print(f"Changed working directory to: {working_dir}")
            except (FileNotFoundError, NotADirectoryError):
                print(f"Error: Invalid working directory: {working_dir}")
                return False

        # Perform provisioning operations
        print("Provisioning in progress...")
        print(f"Current working directory: {os.getcwd()}")

        # Simulate some provisioning work
        print("Provisioning completed successfully.")

        # Change back to original directory if it was changed
        if working_dir:
            os.chdir(original_dir)

        return True

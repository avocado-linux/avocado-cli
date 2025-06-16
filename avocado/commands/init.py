"""Init command implementation."""
import os
import sys
import argparse
import tomlkit
from avocado.commands.base import BaseCommand


class InitCommand(BaseCommand):
    """Implementation of the 'init' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the init command's subparser."""
        parser = subparsers.add_parser(
            "init",
            help="Initialize a new avocado project"
        )

        # Required arguments
        parser.add_argument(
            "-t", "--target",
            required=True,
            help="Target architecture/board (e.g., qemux86-64)"
        )

        parser.add_argument(
            "-s", "--sdk-image",
            required=True,
            help="SDK image to use (e.g., avocadolinux/sdk:x86_64-dev)"
        )

        # Optional argument - the directory to initialize
        parser.add_argument(
            "directory",
            nargs="?",
            default=".",
            help="Directory to initialize (defaults to current directory)"
        )

        return parser

    def execute(self, args, parser=None):
        """Execute the init command."""
        target = args.target
        sdk_image = args.sdk_image
        directory = args.directory

        # Validate the directory
        if not os.path.exists(directory):
            try:
                os.makedirs(directory)
                print(f"Created directory: {directory}")
            except OSError as e:
                print(f"Error creating directory {
                      directory}: {str(e)}", file=sys.stderr)
                return False

        # Create the avocado.toml file
        toml_path = os.path.join(directory, "avocado.toml")

        # Check if configuration file already exists
        if os.path.exists(toml_path):
            print(f"Error: Configuration file '{toml_path}' already exists.", file=sys.stderr)
            print("Use an empty directory or edit the existing configuration file manually.", file=sys.stderr)
            return False

        try:
            # Create TOML document with tomlkit
            doc = tomlkit.document()

            # Add runtime section
            runtime_table = tomlkit.table()
            runtime_table.add("target", target)
            doc.add("runtime", runtime_table)

            # Add empty line for readability
            doc.add(tomlkit.nl())

            # Add sdk section
            sdk_table = tomlkit.table()
            sdk_table.add("image", sdk_image)
            doc.add("sdk", sdk_table)

            # Write to file
            with open(toml_path, 'w') as f:
                f.write(tomlkit.dumps(doc))

            print(f"Initialized avocado project in {
                  os.path.abspath(directory)}")
            print(f"Created configuration file: {os.path.abspath(toml_path)}")
            return True

        except Exception as e:
            print(f"Error creating avocado.toml: {str(e)}", file=sys.stderr)
            return False

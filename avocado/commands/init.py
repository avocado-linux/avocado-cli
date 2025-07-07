"""Init command implementation."""
import os
from avocado.commands.base import BaseCommand
from avocado.utils.output import print_error, print_success


class InitCommand(BaseCommand):
    """Implementation of the 'init' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the init command's subparser."""
        parser = subparsers.add_parser(
            "init",
            help="Initialize a new avocado project"
        )

        # No longer need local target argument - uses global target

        # Optional argument - the directory to initialize
        parser.add_argument(
            "directory",
            nargs="?",
            default=".",
            help="Directory to initialize (defaults to current directory)"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the init command."""
        target = args.resolved_target or "qemux86-64"  # Use global target with default fallback
        directory = args.directory

        # Validate the directory
        if not os.path.exists(directory):
            try:
                os.makedirs(directory)
            except OSError as e:
                print_error(f"creating directory {
                    directory}: {str(e)}.")
                return False

        # Create the avocado.toml file
        toml_path = os.path.join(directory, "avocado.toml")

        # Check if configuration file already exists
        if os.path.exists(toml_path):
            print_error(f"Configuration file '{toml_path}' already exists.")
            return False

        try:
            # Create the new configuration template
            config_content = f'''[runtime.default]
target = "{target}"

[runtime.default.dependencies]
nativesdk-avocado-images = "*"

[sdk]
image = "avocadolinux/sdk:apollo-edge"
'''

            # Write to file
            with open(toml_path, 'w') as f:
                f.write(config_content)

            print_success(f"Created config at {os.path.abspath(toml_path)}.")
            return True

        except Exception as e:
            print_error(f"creating avocado.toml: {str(e)}.")
            return False

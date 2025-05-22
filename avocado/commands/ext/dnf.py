"""Extension DNF command implementation."""
import sys
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper
from avocado.utils.config import load_config
from avocado.utils.output import print_error, print_info, print_success


class ExtDnfCommand(BaseCommand):
    """Implementation of the 'ext dnf' command."""

    def _do_extension_setup(self, config, extension, container_helper, container_image, target, verbose):
        """Perform extension setup for DNF operations."""
        # Check if extension directory structure exists, create it if not
        check_cmd = f"test -d ${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension}"
        dir_exists = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", check_cmd],
            target=target,
            verbose=False,  # Don't show verbose output for the check
            source_environment=False
        )

        if not dir_exists:
            print_info(f"Creating sysroot for extension '{extension}'.")
            setup_commands = [
                f"mkdir -p ${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension}/var/lib",
                f"cp -rf ${{AVOCADO_SDK_SYSROOTS}}/rootfs/var/lib/rpm ${{AVOCADO_SDK_SYSROOTS}}/extensions/{
                    extension}/var/lib"
            ]

            setup_success = container_helper.run_user_command(
                container_image=container_image,
                command=["bash", "-c", " && ".join(setup_commands)],
                target=target,
                verbose=verbose,
                source_environment=False
            )

            if setup_success:
                print_success(f"Created sysroot for extension '{extension}'.")
            else:
                print_error(f"Failed to create sysroot for extension '{extension}'.")
                return False

        return True

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext dnf command's subparser."""
        parser = subparsers.add_parser(
            "dnf",
            help="Run DNF commands in an extension's context"
        )

        # Add common arguments
        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        # Extension is now required
        parser.add_argument(
            "extension",
            help="Extension name to operate on"
        )

        # Capture all remaining arguments after -- as dnf command
        parser.add_argument(
            "dnf_args",
            nargs="*",
            help="DNF command and arguments to execute (use -- to separate from extension args)"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext dnf command."""
        # Add unknown args to dnf_args if they exist
        dnf_args = getattr(args, 'dnf_args', [])
        if unknown:
            dnf_args.extend(unknown)

        if not dnf_args:
            print(
                "Error: No DNF command specified. Use '--' to separate DNF arguments.", file=sys.stderr)
            if parser:
                parser.print_help()
            return False

        config_path = args.config
        extension = args.extension

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image from configuration
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            print(
                "Error: No container image specified in config under 'sdk.image'", file=sys.stderr)
            return False

        # Get the target architecture from configuration
        target = config.get('runtime', {}).get('target')
        if not target:
            print(
                "Error: No target architecture specified in config under 'runtime.target'", file=sys.stderr)
            return False

        container_helper = SdkContainerHelper()

        # Check if extension exists in configuration
        if "ext" not in config or extension not in config["ext"]:
            print_error(f"Extension '{
                extension}' not found in configuration.")
            return False

        # Do extension setup first
        if not self._do_extension_setup(config, extension, container_helper, container_image, target, False):
            return False

        # Build dnf command with extension installroot
        installroot = f"${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension}"
        dnf_cmd = f"$DNF_SDK_HOST --installroot={
            installroot} {' '.join(dnf_args)}"

        # Create the entrypoint script
        entrypoint_script = container_helper._create_entrypoint_script(
            target, source_environment=False)

        # Create the complete bash command
        bash_cmd = [
            "bash", "-c",
            entrypoint_script + f"\n# Execute DNF command\n{dnf_cmd}"
        ]

        # Run the command
        success = container_helper.runner.run_container_command(
            container_image=container_image,
            command=bash_cmd,
            target=target,
            interactive=True,
            tty=True
        )

        # Log the result
        if success:
            print_success(f"DNF command completed successfully.")
        else:
            print_error(f"DNF command failed.")

        return success

    def _print_help(self):
        """Print custom help message."""
        print(
            "usage: avocado ext dnf [-h] [-c CONFIG] extension -- <dnf_args>...")
        print()
        print("Execute DNF commands in an extension's context")
        print()
        print("positional arguments:")
        print("  extension             Extension name to operate on")
        print()
        print("options:")
        print("  -h, --help            show this help message and exit")
        print("  -c CONFIG, --config CONFIG")
        print("                        Path to avocado.toml configuration file (default: avocado.toml)")
        print()
        print("dnf_args:")
        print("  Any DNF command and arguments to execute")
        print("  Note: Use '--' to separate DNF arguments from extension arguments")
        print()
        print("Examples:")
        print("  avocado ext dnf myext -- repolist --enabled")
        print("  avocado ext dnf myext -- search python")
        print("  avocado ext dnf myext -- install vim")
        print("  avocado ext dnf myext -- --reinstall install gcc")
        print("  avocado ext dnf myext -- list installed")

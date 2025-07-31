"""Extension dnf command implementation."""

import sys
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.config import load_config
from avocado.utils.output import print_error, print_info, print_success
from avocado.utils.target import resolve_target, get_target_from_config


class ExtDnfCommand(BaseCommand):
    """Implementation of the 'ext dnf' command."""

    def _do_extension_setup(
        self, config, extension_name, container_helper, container_image, target, verbose
    ):
        """Perform extension setup for DNF operations."""
        # Check if extension directory structure exists, create it if not
        check_cmd = f"test -d ${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension_name}"
        dir_exists = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=["bash", "-c", check_cmd],
            verbose=False,  # Don't show verbose output for the check
            source_environment=False,
        )

        if not dir_exists:
            print_info(f"Creating sysroot for extension '{extension_name}'.")
            setup_commands = [
                f"mkdir -p ${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension_name}/var/lib",
                f"cp -rf ${{AVOCADO_SDK_SYSROOTS}}/rootfs/var/lib/rpm ${{AVOCADO_SDK_SYSROOTS}}/extensions/{
                    extension_name}/var/lib",
            ]

            setup_success = container_helper.run_in_container(
                container_image=container_image,
                target=target,
                command=["bash", "-c", " && ".join(setup_commands)],
                verbose=verbose,
                source_environment=False,
            )

            if setup_success:
                print_success(f"Created sysroot for extension '{extension_name}'.")
            else:
                print_error(f"Failed to create sysroot for extension '{extension_name}'.")
                return False

        return True

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext dnf command's subparser."""
        parser = subparsers.add_parser(
            "dnf", help="Run DNF commands in an extension's context"
        )

        # Add common arguments
        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        # Extension argument - can be positional or named
        parser.add_argument("extension", nargs="?", help="Extension name to operate on")
        parser.add_argument(
            "-e",
            "--extension",
            dest="extension_named",
            help="Extension name to operate on"
        )

        # Capture all remaining arguments after -- as dnf command
        parser.add_argument(
            "dnf_args",
            nargs="*",
            help="DNF command and arguments to execute (use -- to separate from extension args)",
        )

        parser.add_argument(
            "--container-args",
            nargs="*",
            help="Additional arguments to pass to the container runtime (e.g., volume mounts, port mappings)",
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext dnf command."""
        # Determine extension name from positional or named argument
        extension_name = getattr(args, 'extension_named', None) or args.extension
        if not extension_name:
            print_error("Extension name is required. Provide it positionally or via -e/--extension.")
            return False

        # Add unknown args to dnf_args if they exist
        dnf_args = getattr(args, "dnf_args", [])
        if unknown:
            dnf_args.extend(unknown)

        if not dnf_args:
            print(
                "Error: No DNF command specified. Use '--' to separate DNF arguments.",
                file=sys.stderr,
            )
            if parser:
                parser.print_help()
            return False

        config_path = args.config

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get the SDK image from configuration
        container_image = config.get("sdk", {}).get("image")
        if not container_image:
            print(
                "Error: No container image specified in config under 'sdk.image'",
                file=sys.stderr,
            )
            return False

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target = resolve_target(
            cli_target=args.resolved_target, config_target=config_target
        )
        if not target:
            print(
                "Error: No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.",
                file=sys.stderr,
            )
            return False

        container_helper = SdkContainer()

        # Check if extension exists in configuration
        if "ext" not in config or extension_name not in config["ext"]:
            print_error(
                f"Extension '{
                extension_name}' not found in configuration."
            )
            return False

        # Do extension setup first
        if not self._do_extension_setup(
            config, extension_name, container_helper, container_image, target, False
        ):
            return False

        # Build dnf command with extension installroot
        installroot = f"${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension_name}"
        dnf_cmd = f"$DNF_SDK_HOST --installroot={
            installroot} {' '.join(dnf_args)}"

        # Run the DNF command using the container helper
        success = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=["bash", "-c", dnf_cmd],
            source_environment=False,
            use_entrypoint=True,
            container_args=getattr(args, 'container_args', None),
        )

        # Log the result
        if success:
            print_success(f"DNF command completed successfully.")
        else:
            print_error(f"DNF command failed.")

        return success

    def _print_help(self):
        """Print custom help message."""
        print("usage: avocado ext dnf [-h] [-C CONFIG] extension -- <dnf_args>...")
        print()
        print("Execute DNF commands in an extension's context")
        print()
        print("positional arguments:")
        print("  extension             Extension name to operate on")
        print()
        print("options:")
        print("  -h, --help            show this help message and exit")
        print("  -C CONFIG, --config CONFIG")
        print(
            "                        Path to avocado.toml configuration file (default: avocado.toml)"
        )
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

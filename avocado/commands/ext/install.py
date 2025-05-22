"""Extension install command implementation."""

import os
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainerHelper
from avocado.utils.output import print_success, print_info, print_error
from avocado.utils.config import load_config


class ExtInstallCommand(BaseCommand):
    """Implementation of the 'ext install' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext install command's subparser."""
        parser = subparsers.add_parser(
            "install",
            help="Install dependencies into an extension's sysroot"
        )

        # Add config file argument
        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        parser.add_argument(
            "-v", "--verbose",
            action="store_true",
            help="Enable verbose output"
        )

        # Add extension name argument - optional
        parser.add_argument(
            "extension",
            nargs="?",
            help="Name of the extension to install (if not provided, installs all extensions)"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext install command."""
        extension = args.extension
        config_path = args.config
        verbose = args.verbose

        # Load the configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Check if ext section exists
        if "ext" not in config:
            if extension is not None:
                print_error(f"Extension '{
                            extension}' not found in configuration.")
                return False
            else:
                print_info("No extensions found in configuration.")
                return True

        # Determine which extensions to install
        if extension is not None:
            # Single extension specified
            if extension not in config["ext"]:
                print_error(
                    f"Extension '{extension}' not found in configuration.")
                return False
            extensions_to_install = [extension]
        else:
            # No extension specified - install all extensions
            extensions_to_install = list(config["ext"].keys())
            if not extensions_to_install:
                print_info("No extensions found in configuration.")
                return True

            print_info("Installing {} extension(s): {}.".format(
                       len(extensions_to_install), ', '.join(extensions_to_install)))

        # Get the SDK image and target from configuration
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            print_error(
                "No container image specified in config under 'sdk.image'.")
            return False

        target = config.get('runtime', {}).get('target')
        if not target:
            print_error(
                "No target architecture specified in config under 'runtime.target'.")
            return False

        # Use the container helper to run the setup commands
        container_helper = SdkContainerHelper()

        # Install each extension
        for ext_name in extensions_to_install:
            print_info(f"Installing extension '{ext_name}'.")

            if not self._install_single_extension(config, ext_name, container_helper, container_image, target, verbose):
                return False

            print_info(f"Installed extension '{ext_name}'.")

        if len(extensions_to_install) >= 1:
            print_success(f"Installed {
                          len(extensions_to_install)} extension(s).")

        return True

    def _install_single_extension(self, config, extension, container_helper, container_image, target, verbose):
        """Install a single extension."""
        # Create the commands to check and set up the directory structure
        check_command = f"[ -d ${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension} ]"
        setup_command = f"mkdir -p ${{AVOCADO_SDK_SYSROOTS}}/extensions/{
            extension}/var/lib && cp -rf ${{AVOCADO_SDK_SYSROOTS}}/rootfs/var/lib/rpm ${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension}/var/lib"

        # First check if the sysroot already exists
        sysroot_exists = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", check_command],
            target=target,
            verbose=verbose,
            source_environment=False
        )

        if not sysroot_exists:
            # Create the sysroot
            success = container_helper.run_user_command(
                container_image=container_image,
                command=["bash", "-c", setup_command],
                target=target,
                verbose=verbose,
                source_environment=False
            )

            if success:
                print_success(f"Created sysroot for extension '{extension}'.")
            else:
                print_error(
                    f"Failed to create sysroot for extension '{extension}'.")
                return False

        # Install dependencies if they exist
        extension_config = config["ext"][extension]
        dependencies = extension_config.get("dependencies", {})

        if dependencies:
            print_info(f"Found {len(dependencies)} dependencies for extension '{
                       extension}'.")

            # Build list of packages to install
            packages = []
            for package_name, version in dependencies.items():
                if version == "*":
                    packages.append(package_name)
                else:
                    packages.append(f"{package_name}-{version}")

            if packages:
                print_info(f"Installing packages into sysroot: {
                           ', '.join(packages)}.")

                # Build DNF install command
                installroot = f"${{AVOCADO_SDK_SYSROOTS}}/extensions/{extension}"
                dnf_cmd = f"$DNF_SDK_HOST --installroot={
                    installroot} install -y {' '.join(packages)}"

                if verbose:
                    print_info(f"Running command: {dnf_cmd}.")

                # Create the entrypoint script
                entrypoint_script = container_helper._create_entrypoint_script(
                    target, source_environment=False)

                # Create the complete bash command
                bash_cmd = [
                    "bash", "-c",
                    entrypoint_script +
                    f"\n# Install dependencies\n{dnf_cmd}"
                ]

                # Run the DNF install command
                install_success = container_helper.runner.run_container_command(
                    container_image=container_image,
                    command=bash_cmd,
                    target=target,
                    interactive=False,
                    tty=False
                )

                if install_success:
                    print_info(
                        f"Installed {len(packages)} dependency(s) into extension '{extension}' sysroot: {os.getcwd()}/_avocado/sdk/sysroots/extensions/{extension}.")
                else:
                    print_error(
                        f"Failed to install dependencies for extension '{extension}'.")
                    return False
            else:
                print_info(f"No valid dependencies to install for extension '{
                           extension}'.")
        else:
            print_info(f"No dependencies defined for extension '{extension}'.")

        return True

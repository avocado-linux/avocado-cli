"""Extension install command implementation."""

import os
from avocado.commands.base import BaseCommand
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_success, print_info, print_error, print_debug
from avocado.utils.config import load_config
from avocado.utils.target import resolve_target, get_target_from_config


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

        parser.add_argument(
            "-f", "--force",
            action="store_true",
            help="Force the operation to proceed, bypassing warnings or confirmation prompts."
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

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target = resolve_target(
            cli_target=args.resolved_target, config_target=config_target)
        if not target:
            print_error(
                "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
            return False

        # Use the container helper to run the setup commands
        container_helper = SdkContainer()
        index = 0
        total = len(extensions_to_install)

        # Install each extension
        for ext_name in extensions_to_install:
            index += 1
            if args.verbose:
                print_debug(f"Installing ({index}/{total}) {ext_name}.")

            if not self._install_single_extension(config, ext_name, container_helper,
                                                  container_image, target, verbose,
                                                  args.force):
                return False

        if len(extensions_to_install) >= 1:
            print_success(f"Installed {
                          len(extensions_to_install)} extension(s).")

        return True

    def _install_single_extension(self, config, extension, container_helper,
                                  container_image, target, verbose, force):
        """Install a single extension."""
        # Create the commands to check and set up the directory structure
        check_command = f"[ -d $AVOCADO_EXT_SYSROOTS/{extension} ]"
        setup_command = f"mkdir -p ${{AVOCADO_EXT_SYSROOTS}}/{
            extension}/var/lib && cp -rf ${{AVOCADO_PREFIX}}/rootfs/var/lib/rpm ${{AVOCADO_EXT_SYSROOTS}}/{extension}/var/lib"

        # First check if the sysroot already exists
        sysroot_exists = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=check_command,
            verbose=verbose,
            source_environment=False
        )

        if not sysroot_exists:
            # Create the sysroot
            success = container_helper.run_in_container(
                container_image=container_image,
                target=target,
                command=setup_command,
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
            # Build list of packages to install
            packages = []
            for package_name, version in dependencies.items():
                # Skip compile dependencies (identified by dict value with 'compile' key)
                if isinstance(version, dict) and 'compile' in version:
                    continue

                if version == "*":
                    packages.append(package_name)
                else:
                    packages.append(f"{package_name}-{version}")

            if packages:
                # Build DNF install command
                yes = "-y" if force else ""
                installroot = f"${{AVOCADO_EXT_SYSROOTS}}/{extension}"
                command = f"""
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot={installroot} \
    install \
    {yes} \
    {' '.join(packages)}
"""

                if verbose:
                    print_info(f"Running command: {command}.")

                # Run the DNF install command
                install_success = container_helper.run_in_container(
                    container_image=container_image,
                    target=target,
                    command=command,
                    source_environment=False,
                    use_entrypoint=True
                )

                if not install_success:
                    print_error(
                        f"Failed to install dependencies for extension '{extension}'.")
                    return False
            else:
                if verbose:
                    print_debug(f"No valid dependencies to install for extension '{
                               extension}'.")
        else:
            if verbose:
                print_debug(f"No dependencies defined for extension '{extension}'.")

        return True

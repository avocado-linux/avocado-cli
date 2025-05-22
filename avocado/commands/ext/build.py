"""Extension build command implementation."""
import os
import sys
import subprocess
import tomlkit
from pathlib import Path
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainerHelper
from avocado.utils.output import print_error, print_success, print_info


class ExtBuildCommand(BaseCommand):
    """Implementation of the 'ext build' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext build command's subparser."""
        parser = subparsers.add_parser(
            "build",
            help="Build sysext and/or confext extensions from configuration"
        )

        # Add extension name argument - required
        parser.add_argument(
            "extension",
            help="Name of the extension to build (must be defined in config)"
        )

        # Add optional arguments
        parser.add_argument(
            "--verbose", "-v",
            action="store_true",
            help="Enable verbose output"
        )

        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the ext build command."""
        extension_name = args.extension
        config_path = args.config
        verbose = args.verbose

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get extension configuration
        ext_config = config.get('ext', {}).get(extension_name)
        if not ext_config:
            print_error(
                f"Extension '{extension_name}' not found in configuration.")
            return False

        # Get extension types (sysext, confext) from boolean flags
        ext_types = []
        if ext_config.get('sysext', False):
            ext_types.append('sysext')
        if ext_config.get('confext', False):
            ext_types.append('confext')

        if not ext_types:
            print_error(
                f"Extension '{extension_name}' has sysext=false and confext=false. At least one must be true to build.")
            return False

        # Get extension version
        ext_version = ext_config.get('version', '0.1.0')

        # Get SDK configuration
        sdk_config = config.get('sdk', {})
        container_image = sdk_config.get('image')
        if not container_image:
            print_error("No SDK container image specified in configuration.")
            return False

        # Get runtime configuration
        runtime_config = config.get('runtime', {})
        target_arch = runtime_config.get('target')
        if not target_arch:
            print_error("No target architecture specified in configuration.")
            return False

        # Initialize SDK container helper
        container_helper = SdkContainerHelper(verbose=verbose)

        # Build extensions based on configuration
        overall_success = True

        for ext_type in ext_types:
            print_info(f"Building {ext_type} extension '{extension_name}'.")

            if ext_type == 'sysext':
                build_result = self._build_sysext_extension(
                    container_helper, container_image, target_arch,
                    extension_name, ext_version, verbose
                )
            elif ext_type == 'confext':
                build_result = self._build_confext_extension(
                    container_helper, container_image, target_arch,
                    extension_name, ext_version, verbose
                )

            if build_result:
                print_success(f"Successfully built {
                              ext_type} extension '{extension_name}'.")
            else:
                print_error(f"Failed to build {
                            ext_type} extension '{extension_name}'.")
                overall_success = False

        return overall_success

    def _build_sysext_extension(self, container_helper, container_image, target_arch,
                                extension_name, ext_version, verbose):
        """Build a sysext extension."""

        # Create the build script for sysext extension
        build_script = self._create_sysext_build_script(
            extension_name, ext_version)

        # Execute the build script in the SDK container
        if verbose:
            print_info(f"Executing sysext extension build script.")

        result = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", build_script],
            target=target_arch,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        if verbose:
            print_info(f"Sysext build script execution returned: {result}.")

        return result

    def _build_confext_extension(self, container_helper, container_image, target_arch,
                                 extension_name, ext_version, verbose):
        """Build a confext extension."""

        # Create the build script for confext extension
        build_script = self._create_confext_build_script(
            extension_name, ext_version)

        # Execute the build script in the SDK container
        if verbose:
            print_info(f"Executing confext extension build script.")

        result = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", build_script],
            target=target_arch,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        if verbose:
            print_info(f"Confext build script execution returned: {result}.")

        return result

    def _create_sysext_build_script(self, extension_name, ext_version):
        """Create the build script for sysext extension."""

        # Common variables
        ext_id = "avocado"

        script = f'''
set -e

release_dir="$AVOCADO_SDK_SYSROOTS/sysext/usr/lib/extension-release.d"
release_file="$release_dir/extension-release.{extension_name}"

mkdir -p "$release_dir"
echo "ID={ext_id}" > "$release_file"
echo "VERSION_ID={ext_version}" >> "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
'''

        return script

    def _create_confext_build_script(self, extension_name, ext_version):
        """Create the build script for confext extension."""

        # Common variables
        ext_id = "avocado"

        script = f'''
set -e

release_dir="$AVOCADO_SDK_SYSROOTS/confext/etc/extension-release.d"
release_file="$release_dir/extension-release.{extension_name}"

mkdir -p "$release_dir"
echo "ID={ext_id}" > "$release_file"
echo "VERSION_ID={ext_version}" >> "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
'''

        return script

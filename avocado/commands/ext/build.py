"""Extension build command implementation."""

import os
import sys
import subprocess
import tomlkit
from pathlib import Path
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_error, print_success, print_info
from avocado.utils.target import resolve_target, get_target_from_config


class ExtBuildCommand(BaseCommand):
    """Implementation of the 'ext build' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext build command's subparser."""
        parser = subparsers.add_parser(
            "build", help="Build sysext and/or confext extensions from configuration"
        )

        # Add extension name argument - required
        parser.add_argument(
            "extension",
            help="Name of the extension to build (must be defined in config)",
        )

        # Add optional arguments
        parser.add_argument(
            "--verbose", "-v", action="store_true", help="Enable verbose output"
        )

        parser.add_argument(
            "-c",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
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
        ext_config = config.get("ext", {}).get(extension_name)
        if not ext_config:
            print_error(f"Extension '{extension_name}' not found in configuration.")
            return False

        # Get extension types (sysext, confext) from boolean flags
        ext_types = []
        if ext_config.get("sysext", False):
            ext_types.append("sysext")
        if ext_config.get("confext", False):
            ext_types.append("confext")

        ext_scopes = ext_config.get("scopes", ["system"])
        sysext_scopes = ext_config.get("sysext_scopes", ext_scopes)
        confext_scopes = ext_config.get("confext_scopes", ext_scopes)

        if not ext_types:
            print_error(
                f"Extension '{extension_name}' has sysext=false and confext=false. At least one must be true to build."
            )
            return False

        # Get extension version
        ext_version = ext_config.get("version", "0.1.0")

        # Get SDK configuration
        sdk_config = config.get("sdk", {})
        container_image = sdk_config.get("image")
        if not container_image:
            print_error("No SDK container image specified in configuration.")
            return False

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target_arch = resolve_target(
            cli_target=args.resolved_target, config_target=config_target
        )
        if not target_arch:
            print_error(
                "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'."
            )
            return False

        # Initialize SDK container helper
        container_helper = SdkContainer(verbose=verbose)

        # Build extensions based on configuration
        overall_success = True

        for ext_type in ext_types:
            print_info(f"Building {ext_type} extension '{extension_name}'.")

            if ext_type == "sysext":
                build_result = self._build_sysext_extension(
                    container_helper,
                    container_image,
                    target_arch,
                    extension_name,
                    ext_version,
                    sysext_scopes,
                    verbose,
                )
            elif ext_type == "confext":
                build_result = self._build_confext_extension(
                    container_helper,
                    container_image,
                    target_arch,
                    extension_name,
                    ext_version,
                    confext_scopes,
                    verbose,
                )

            if build_result:
                print_success(
                    f"Successfully built {
                              ext_type} extension '{extension_name}'."
                )
            else:
                print_error(
                    f"Failed to build {
                            ext_type} extension '{extension_name}'."
                )
                overall_success = False

        return overall_success

    def _build_sysext_extension(
        self,
        container_helper,
        container_image,
        target_arch,
        extension_name,
        ext_version,
        ext_scopes,
        verbose,
    ):
        """Build a sysext extension."""

        # Create the build script for sysext extension
        build_script = self._create_sysext_build_script(
            extension_name, ext_version, ext_scopes
        )

        # Execute the build script in the SDK container
        if verbose:
            print_info(f"Executing sysext extension build script.")

        result = container_helper.run_in_container(
            container_image=container_image,
            target=target_arch,
            command=build_script,
            rm=True,
            verbose=verbose,
            source_environment=True,
        )

        if verbose:
            print_info(f"Sysext build script execution returned: {result}.")

        return result

    def _build_confext_extension(
        self,
        container_helper,
        container_image,
        target_arch,
        extension_name,
        ext_version,
        ext_scopes,
        verbose,
    ):
        """Build a confext extension."""

        # Create the build script for confext extension
        build_script = self._create_confext_build_script(
            extension_name, ext_version, ext_scopes
        )

        # Execute the build script in the SDK container
        if verbose:
            print_info(f"Executing confext extension build script.")

        result = container_helper.run_in_container(
            container_image=container_image,
            target=target_arch,
            command=build_script,
            rm=True,
            verbose=verbose,
            source_environment=True,
        )

        if verbose:
            print_info(f"Confext build script execution returned: {result}.")

        return result

    def _create_sysext_build_script(self, extension_name, ext_version, ext_scopes):
        """Create the build script for sysext extension."""

        # Common variables
        script = f"""
set -e

release_dir="$AVOCADO_EXT_SYSROOTS/{extension_name}/usr/lib/extension-release.d"
release_file="$release_dir/extension-release.{extension_name}"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
echo "SYSEXT_SCOPE={" ".join(ext_scopes)}" >> "$release_file"
"""

        return script

    def _create_confext_build_script(self, extension_name, ext_version, ext_scopes):
        """Create the build script for confext extension."""

        # Common variables
        script = f"""
set -e

release_dir="$AVOCADO_EXT_SYSROOTS/{extension_name}/etc/extension-release.d"
release_file="$release_dir/extension-release.{extension_name}"

mkdir -p "$release_dir"
echo "ID=_any" > "$release_file"
echo "EXTENSION_RELOAD_MANAGER=1" >> "$release_file"
echo "CONFEXT_SCOPE={" ".join(ext_scopes)}" >> "$release_file"
"""

        return script

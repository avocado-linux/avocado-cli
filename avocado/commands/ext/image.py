"""Extension image command implementation."""
import os
import sys
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainerHelper
from avocado.utils.output import print_error, print_success, print_info


class ExtImageCommand(BaseCommand):
    """Implementation of the 'ext image' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext image command's subparser."""
        parser = subparsers.add_parser(
            "image",
            help="Create squashfs image from system extension"
        )

        # Add extension name argument - required
        parser.add_argument(
            "extension",
            help="Name of the extension to create image for"
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
        """Execute the ext image command."""
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
                f"Extension '{extension_name}' has sysext=false and confext=false. At least one must be true to create image.")
            return False

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

        # Create images based on configuration
        overall_success = True

        for ext_type in ext_types:
            print_info(f"Creating {ext_type} image for extension '{
                       extension_name}'.")

            if ext_type == 'sysext':
                result = self._create_sysext_image(
                    container_helper, container_image, target_arch,
                    extension_name, verbose
                )
            elif ext_type == 'confext':
                result = self._create_confext_image(
                    container_helper, container_image, target_arch,
                    extension_name, verbose
                )

            if result:
                print_success(f"Successfully created {ext_type} image for extension '{
                              extension_name}'.")
            else:
                print_error(f"Failed to create {ext_type} image for extension '{
                            extension_name}'.")
                overall_success = False

        return overall_success

    def _create_sysext_image(self, container_helper, container_image, target_arch,
                             extension_name, verbose):
        """Create a sysext squashfs image."""

        # Create the sysext build script
        build_script = self._create_sysext_build_script(extension_name)

        # Execute the build script in the SDK container
        if verbose:
            print_info("Executing sysext image build script.")

        result = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", build_script],
            target=target_arch,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        return result

    def _create_confext_image(self, container_helper, container_image, target_arch,
                              extension_name, verbose):
        """Create a confext squashfs image."""

        # Create the confext build script
        build_script = self._create_confext_build_script(extension_name)

        # Execute the build script in the SDK container
        if verbose:
            print_info("Executing confext image build script.")

        result = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", build_script],
            target=target_arch,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        return result

    def _create_sysext_build_script(self, extension_name):
        """Create the sysext image build script."""

        script = f'''
set -e

# Common variables
OUTPUT_DIR="/opt/_avocado/extensions"
EXT_NAME="{extension_name}"
OUTPUT_FILE="${{OUTPUT_DIR}}/sysext/${{EXT_NAME}}.raw"

# Create output directory
mkdir -p "${{OUTPUT_DIR}}/sysext"

# Remove existing file if it exists
rm -f "$OUTPUT_FILE"

# Check if sysext directory exists
if [ ! -d "$AVOCADO_SDK_SYSROOTS/extensions/${{EXT_NAME}}" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_SDK_SYSROOTS/extensions/${{EXT_NAME}}."
    exit 1
fi

# Create squashfs image
mksquashfs "$AVOCADO_SDK_SYSROOTS/sysext" "$OUTPUT_FILE" -noappend
'''

        return script

    def _create_confext_build_script(self, extension_name):
        """Create the confext image build script."""

        script = f'''
set -e

# Common variables
OUTPUT_DIR="/opt/_avocado/extensions"
EXT_NAME="{extension_name}"
OUTPUT_FILE="${{OUTPUT_DIR}}/confext/${{EXT_NAME}}.raw"

# Create output directory
mkdir -p "${{OUTPUT_DIR}}/confext"

# Remove existing file if it exists
rm -f "$OUTPUT_FILE"

# Check if confext directory exists
if [ ! -d "$AVOCADO_SDK_SYSROOTS/extensions/${{EXT_NAME}}" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_SDK_SYSROOTS/extensions/${{EXT_NAME}}."
    exit 1
fi

# Create squashfs image
mksquashfs "$AVOCADO_SDK_SYSROOTS/confext" "$OUTPUT_FILE" -noappend
'''

        return script

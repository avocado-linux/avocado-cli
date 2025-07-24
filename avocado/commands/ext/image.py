"""Extension image command implementation."""
import os
import sys
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_error, print_success, print_info
from avocado.utils.target import resolve_target, get_target_from_config


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

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = get_target_from_config(config)
        target_arch = resolve_target(
            cli_target=args.resolved_target, config_target=config_target)
        if not target_arch:
            print_error(
                "No target architecture specified. Use --target, AVOCADO_TARGET env var, or config under 'runtime.<name>.target'.")
            return False

        # Initialize SDK container helper
        container_helper = SdkContainer(verbose=verbose)

        # Create images based on configuration
        overall_success = True

        for ext_type in ext_types:
            print_info(f"Creating {ext_type} image for extension '{
                       extension_name}'.")

            result = self._create_image(
                container_helper, container_image, target_arch,
                extension_name, ext_type, verbose
            )

            if result:
                print_success(f"Successfully created {ext_type} image for extension '{
                              extension_name}'.")
            else:
                print_error(f"Failed to create {ext_type} image for extension '{
                            extension_name}'.")
                overall_success = False

        return overall_success

    def _create_image(self, container_helper, container_image, target_arch,
                      extension_name, extension_type, verbose):
        # Create the build script
        build_script = self._create_build_script(
            extension_name, extension_type)

        # Execute the build script in the SDK container
        if verbose:
            print_info("Executing image build script.")

        result = container_helper.run_in_container(
            container_image=container_image,
            target=target_arch,
            command=build_script,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        return result

    def _create_build_script(self, extension_name, extension_type):
        """Create the image build script."""

        script = f'''
set -e

# Common variables
EXT_NAME="{extension_name}"
OUTPUT_DIR="$AVOCADO_PREFIX/output/extensions"
OUTPUT_FILE="$OUTPUT_DIR/$EXT_NAME.raw"

# Create output directory
mkdir -p $OUTPUT_DIR

# Remove existing file if it exists
rm -f "$OUTPUT_FILE"

# Check if extension directory exists
if [ ! -d "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" ]; then
    echo "Extension sysroot does not exist: $AVOCADO_EXT_SYSROOTS/$EXT_NAME."
    exit 1
fi

# Create squashfs image
mksquashfs \
  "$AVOCADO_EXT_SYSROOTS/$EXT_NAME" \
  "$OUTPUT_FILE" \
  -noappend \
  -no-xattrs
'''

        return script

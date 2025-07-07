"""Runtime build command implementation."""
import os
import sys
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainer
from avocado.utils.output import print_error, print_success, print_info
from avocado.utils.target import resolve_target, get_target_from_config


class RuntimeBuildCommand(BaseCommand):
    """Implementation of the 'runtime build' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the runtime build command's subparser."""
        parser = subparsers.add_parser(
            "build",
            help="Build a runtime"
        )

        # Optional arguments
        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )

        parser.add_argument(
            "--verbose", "-v",
            action="store_true",
            help="Enable verbose output"
        )

        parser.add_argument(
            "-f", "--force",
            action="store_true",
            help="Force the operation to proceed, bypassing warnings or confirmation prompts."
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the runtime build command."""
        config_path = args.config
        verbose = args.verbose

        # Load configuration
        config, success = load_config(config_path)
        if not success:
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

        print_info("Building runtime images.")

        # Initialize SDK container helper
        container_helper = SdkContainer(verbose=verbose)

        # First check if the required images package is already installed (silent check)
        dnf_check_script = '''
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
$DNF_SDK_HOST_OPTS \
$DNF_SDK_TARGET_REPO_CONF \
--installroot=$AVOCADO_PREFIX/images \
list installed avocado-pkg-images >/dev/null 2>&1
'''

        # Use container helper to check package status
        command = dnf_check_script

        package_installed = container_helper.run_in_container(
            container_image=container_image,
            target=target_arch,
            command=command,
            rm=True
        )

        if not package_installed:
            print_info("Installing avocado-pkg-images package.")
            yes = "-y" if args.force else ""

            # Create DNF install script
            dnf_install_script = f'''
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot=$AVOCADO_PREFIX/images \
    install \
    {yes} \
    avocado-pkg-images
'''

            # Run the DNF install command
            install_result = container_helper.run_in_container(
                container_image=container_image,
                target=target_arch,
                command=dnf_install_script,
                rm=True,
            )

            if not install_result:
                print_error(
                    "Failed to install avocado-pkg-images package.")
                return False

            print_success(
                "Successfully installed avocado-pkg-images package.")

        # Build var image first
        var_result = self._build_var_image(
            container_helper, container_image, target_arch, config, verbose
        )

        if not var_result:
            print_error("Failed to build var image.")
            return False

        # Build complete image
        complete_result = self._build_complete_image(
            container_helper, container_image, target_arch, verbose
        )

        if not complete_result:
            print_error("Failed to build complete image.")
            return False

        print_success("Successfully built runtime images.")
        return True

    def _build_var_image(self, container_helper, container_image, target_arch, config, verbose):
        """Build a var image."""
        print_info("Building var image.")

        # Create the var build script
        build_script = self._create_var_build_script(config)

        # Execute the build script in the SDK container
        if verbose:
            print_info("Executing var image build script.")

        result = container_helper.run_in_container(
            container_image=container_image,
            target=target_arch,
            command=build_script,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        if result:
            print_success("Successfully built var image.")
        else:
            print_error("Failed to build var image.")

        return result

    def _build_complete_image(self, container_helper, container_image, target_arch, verbose):
        """Build a complete system image."""
        print_info("Building complete system image.")

        # Create the complete image build script
        build_script = self._create_complete_image_build_script(target_arch)

        # Execute the build script in the SDK container
        if verbose:
            print_info("Executing complete image build script.")

        result = container_helper.run_in_container(
            container_image=container_image,
            target=target_arch,
            command=build_script,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        if result:
            print_success("Successfully built complete system image.")
        else:
            print_error("Failed to build complete system image.")

        return result

    def _create_var_build_script(self, config):
        """Create the var image build script."""

        # Build extension symlink commands from config
        symlink_commands = []
        ext_config = config.get('ext', {})

        for ext_name, ext_data in ext_config.items():
            if isinstance(ext_data, dict):
                is_sysext = ext_data.get('sysext', False)
                is_confext = ext_data.get('confext', False)

                symlink_commands.append(f'''
# The output of ext image.
OUTPUT_EXT=$AVOCADO_PREFIX/output/extensions/{ext_name}.raw
# Where in the runtime sysroot the OUTPUT_EXT should be hard linked to.
RUNTIMES_EXT=$AVOCADO_PREFIX/runtimes/{runtime_name}/var/lib/avocado/extensions/{ext_name}.raw
# Where in the runtime sysroot to symlink the RUNTIMES_EXT to as a system extension.
SYSEXT=$AVOCADO_PREFIX/runtimes/{runtime_name}/var/lib/extensions/{ext_name}.raw
# Where in the runtime sysroot to symlink the RUNTIMES_EXT to as a config extension.
CONFEXT=$AVOCADO_PREFIX/runtimes/{runtime_name}/var/lib/confexts/{ext_name}.raw

if [ -f "$OUTPUT_EXT" ]; then
    if ! cmp -s "$OUTPUT_EXT" "$RUNTIMES_EXT" 2>/dev/null; then
        ln $OUTPUT_EXT $RUNTIMES_EXT
    fi
else
    echo "Missing image for extension {ext_name}."
fi''')

                if is_sysext:
                    symlink_commands.append("ln -sf $RUNTIMES_EXT $SYSEXT"

                if is_confext:
                    symlink_commands.append("ln -sf $RUNTIMES_EXT $CONFEXT"

        symlink_section = '\n'.join(
            symlink_commands) if symlink_commands else '# No extensions configured for symlinking'

        script = '''
set -e

# Common variables
IMAGES_DIR="$AVOCADO_PREFIX/images"
OUTPUT_DIR="$AVOCADO_PREFIX/extensions"

mkdir -p "$IMAGES_DIR"

# Create var sysroot structure
echo "Creating var sysroot structure."
mkdir -p "$AVOCADO_PREFIX/var/lib/extensions"
mkdir -p "$AVOCADO_PREFIX/var/lib/confexts"
mkdir -p "$AVOCADO_PREFIX/var/lib/avocado/extensions"

# Create symlinks based on extension configuration
echo "Creating extension symlinks."
''' + symlink_section + '''

# Create btrfs image with extensions and confexts subvolumes
echo "Creating btrfs image with subvolumes."
mkfs.btrfs -r "$AVOCADO_PREFIX/var" \\
    --subvol rw:lib/extensions \\
    --subvol rw:lib/confexts \\
    -f "${IMAGES_DIR}/avocado-image-var.btrfs"

echo "Successfully created var image: ${IMAGES_DIR}/avocado-image-var.btrfs"
'''

        return script

    def _create_complete_image_build_script(self, target_arch):
        """Create the complete system image build script."""

        script = f'''
set -e

echo "Building complete system image"

# Common variables
IMAGES_DIR="$AVOCADO_PREFIX/images"
DEPLOY_DIR="$IMAGES_DIR/deploy"
OUTPUT_PATH="$AVOCADO_PREFIX/output"
TMP_PATH="$AVOCADO_PREFIX/genimage-tmp"

mkdir -p "$IMAGES_DIR"
mkdir -p "$DEPLOY_DIR"
mkdir -p "$OUTPUT_PATH"

# Clean and recreate temporary directory
rm -rf "$TMP_PATH"
mkdir -p "$TMP_PATH"

# Copy var image to deploy directory
echo "Copying var image to deploy directory..."
cp "${{IMAGES_DIR}}/avocado-image-var.btrfs" "${{DEPLOY_DIR}}/"

# Prepare genimage configuration
echo "Preparing genimage configuration..."
cp "${{DEPLOY_DIR}}/genimage.cfg" "${{OUTPUT_PATH}}/genimage.cfg"

# Replace variables in genimage configuration
sed -i "s|@VAR-IMG@|${{DEPLOY_DIR}}/avocado-image-var.btrfs|g" "${{OUTPUT_PATH}}/genimage.cfg"
sed -i "s|@IMAGE@|avocado-image-{target_arch}.img|g" "${{OUTPUT_PATH}}/genimage.cfg"

# Run genimage to create the final system image
echo "Running genimage to create final system image..."
genimage --config "${{OUTPUT_PATH}}/genimage.cfg" \\
        --inputpath "$DEPLOY_DIR" \\
        --outputpath "$OUTPUT_PATH" \\
        --tmppath "$TMP_PATH" \\
        --rootpath "$IMAGES_DIR"

echo "System image created in $OUTPUT_PATH"
'''

        return script

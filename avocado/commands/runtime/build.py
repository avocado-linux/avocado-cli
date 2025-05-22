"""Runtime build command implementation."""
import os
import sys
from avocado.commands.base import BaseCommand
from avocado.utils.config import load_config
from avocado.utils.container import SdkContainerHelper
from avocado.utils.output import print_error, print_success, print_info


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
        target_arch = runtime_config.get('target')
        if not target_arch:
            print_error("No target architecture specified in configuration.")
            return False

        print_info("Building runtime images.")

        # Initialize SDK container helper
        container_helper = SdkContainerHelper(verbose=verbose)

        # First check if the required images package is already installed (silent check)
        dnf_check_script = '''
$DNF_SDK_HOST --installroot=/opt/_avocado/images list installed avocado-pkg-images >/dev/null 2>&1
'''

        # Use container runner directly to check package status
        from avocado.utils.container import ContainerRunner
        container_runner = ContainerRunner(verbose=verbose)

        # Get the entrypoint script content
        entrypoint_script = container_helper._create_entrypoint_script(
            target_arch, True)

        # Create the complete check command
        check_cmd = ["bash", "-c",
                     entrypoint_script + "\n" + dnf_check_script]

        package_installed = container_runner.run_container_command(
            container_image=container_image,
            command=check_cmd,
            target=target_arch,
            interactive=False,
            rm=True
        )

        if not package_installed:
            print_info("Installing avocado-pkg-images package.")

            # Create DNF install script
            dnf_install_script = '''
$DNF_SDK_HOST --installroot=/opt/_avocado/images install avocado-pkg-images
'''

            # Create the complete interactive command
            interactive_cmd = ["bash", "-c",
                               entrypoint_script + "\n" + dnf_install_script]

            install_result = container_runner.run_container_command(
                container_image=container_image,
                command=interactive_cmd,
                target=target_arch,
                interactive=True,
                rm=True
            )

            if not install_result:
                print_error(
                    "Failed to install avocado-pkg-images package.")
                return False

            print_success(
                "Successfully installed avocado-pkg-images package.")

        # Build var image first
        var_result = self._build_var_image(
            container_helper, container_image, target_arch, verbose
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

    def _build_var_image(self, container_helper, container_image, target_arch, verbose):
        """Build a var image."""
        print_info("Building var image.")

        # Create the var build script
        build_script = self._create_var_build_script()

        # Execute the build script in the SDK container
        if verbose:
            print_info("Executing var image build script.")

        result = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", build_script],
            target=target_arch,
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

        result = container_helper.run_user_command(
            container_image=container_image,
            command=["bash", "-c", build_script],
            target=target_arch,
            rm=True,
            verbose=verbose,
            source_environment=True
        )

        if result:
            print_success("Successfully built complete system image.")
        else:
            print_error("Failed to build complete system image.")

        return result

    def _create_var_build_script(self):
        """Create the var image build script."""

        script = '''
set -e

echo "Building var image"

# Common variables
IMAGES_DIR="/opt/_avocado/images"
OUTPUT_DIR="/opt/_avocado/extensions"

mkdir -p "$IMAGES_DIR"

# Create var sysroot structure
echo "Creating var sysroot structure..."
mkdir -p "$AVOCADO_SDK_SYSROOTS/var/lib/extensions"
mkdir -p "$AVOCADO_SDK_SYSROOTS/var/lib/confexts"

# Copy existing extensions into var sysroot if they exist
echo "Copying system extensions..."
if ls "${OUTPUT_DIR}/sysext/"*.raw 1> /dev/null 2>&1; then
    cp -f "${OUTPUT_DIR}/sysext/"*.raw "$AVOCADO_SDK_SYSROOTS/var/lib/extensions/"
else
    echo "No system extensions found, skipping..."
fi

echo "Copying configuration extensions..."
if ls "${OUTPUT_DIR}/confext/"*.raw 1> /dev/null 2>&1; then
    cp -f "${OUTPUT_DIR}/confext/"*.raw "$AVOCADO_SDK_SYSROOTS/var/lib/confexts/"
else
    echo "No configuration extensions found, skipping..."
fi

# Create btrfs image with extensions and confexts subvolumes
echo "Creating btrfs image with subvolumes..."
mkfs.btrfs -r "$AVOCADO_SDK_SYSROOTS/var" \\
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
IMAGES_DIR="/opt/_avocado/images"
DEPLOY_DIR="${{IMAGES_DIR}}/deploy"
OUTPUT_PATH="/opt/_avocado/output"
TMP_PATH="/opt/_avocado/genimage-tmp"

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

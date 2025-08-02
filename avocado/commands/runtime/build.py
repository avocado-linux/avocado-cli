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
        parser = subparsers.add_parser("build", help="Build a runtime")

        # Runtime argument - can be positional or named
        parser.add_argument("runtime", nargs="?", help="Runtime name to build")
        parser.add_argument(
            "-r",
            "--runtime",
            dest="runtime_named",
            help="Runtime name to build"
        )

        # Optional arguments
        parser.add_argument(
            "-C",
            "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)",
        )

        parser.add_argument(
            "--verbose", "-v", action="store_true", help="Enable verbose output"
        )

        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="Force the operation to proceed, bypassing warnings or confirmation prompts.",
        )

        parser.add_argument(
            "--container-args",
            nargs=1,
            action="append",
            help="Additional arguments to pass to the container runtime (e.g., volume mounts, port mappings)",
            dest='container_args'
        )

        return parser

    def execute(self, args, parser=None, unknown=None):
        """Execute the runtime build command."""
        config_path = args.config
        verbose = args.verbose

        # Determine runtime name from positional or named argument
        runtime_name = getattr(args, 'runtime_named', None) or args.runtime
        if not runtime_name:
            print_error("Runtime name is required. Provide it positionally or via -r/--runtime.")
            return False

        # Load configuration
        config, success = load_config(config_path)
        if not success:
            return False

        # Get SDK configuration
        sdk_config = config.get("sdk", {})
        container_image = sdk_config.get("image")
        if not container_image:
            print_error("No SDK container image specified in configuration.")
            return False

        # Get runtime configuration
        runtime_config = config.get("runtime", {})

        # Check if runtime exists
        if runtime_name not in runtime_config:
            print_error(f"Runtime '{runtime_name}' not found in configuration.")
            return False

        # Use resolved target (from CLI/env) if available, otherwise fall back to config
        config_target = runtime_config[runtime_name].get("target")
        target = resolve_target(
            cli_target=args.resolved_target, config_target=config_target
        )
        if not target:
            print_error(
                f"No target specified for runtime '{runtime_name}'. Use --target, AVOCADO_TARGET env var, or config under 'runtime.{runtime_name}.target'."
            )
            return False

        print_info(f"Building runtime images for '{runtime_name}'.")

        # Initialize SDK container helper
        container_helper = SdkContainer()

        # First check if the required images package is already installed (silent check)
        dnf_check_script = f"""
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
$DNF_SDK_HOST_OPTS \
$DNF_SDK_TARGET_REPO_CONF \
--installroot=$AVOCADO_PREFIX/runtimes/{runtime_name} \
list installed avocado-pkg-images >/dev/null 2>&1
"""

        # Use container helper to check package status
        command = dnf_check_script

        package_installed = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=command,
            rm=True,
            verbose=verbose,
            container_args=getattr(args, 'container_args', None),
        )

        if not package_installed:
            print_info("Installing avocado-pkg-images package.")
            yes = "-y" if args.force else ""

            # Create DNF install script
            dnf_install_script = f"""
RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_TARGET_REPO_CONF \
    --installroot=$AVOCADO_PREFIX/runtimes/{runtime_name} \
    install \
    {yes} \
    avocado-pkg-images
"""
            # Run the DNF install command
            install_result = container_helper.run_in_container(
                container_image=container_image,
                target=target,
                command=dnf_install_script,
                rm=True,
                verbose=verbose,
                interactive=not args.force,
                container_args=getattr(args, 'container_args', None),
            )

            if not install_result:
                print_error("Failed to install avocado-pkg-images package.")
                return False

            print_success("Successfully installed avocado-pkg-images package.")
        else:
            print_info("avocado-pkg-images already installed.")

        # Build var image first
        build_script = self._create_build_script(config, target, runtime_name)

        if verbose:
            print_info("Executing complete image build script.")

        complete_result = container_helper.run_in_container(
            container_image=container_image,
            target=target,
            command=build_script,
            rm=True,
            verbose=verbose,
            source_environment=True,
            container_args=getattr(args, 'container_args', None),
        )

        if not complete_result:
            print_error("Failed to build complete image.")
            return False

        print_success(f"Successfully built runtime '{runtime_name}'.")
        return True

    def _create_build_script(self, config, target, runtime_name):
        # Get runtime dependencies to identify required extensions
        runtime_config = config.get("runtime", {}).get(runtime_name, {})
        runtime_deps = runtime_config.get("dependencies", {})

        # Extract extension names from runtime dependencies
        required_extensions = set()
        for dep_name, dep_spec in runtime_deps.items():
            if isinstance(dep_spec, dict) and "ext" in dep_spec:
                required_extensions.add(dep_spec["ext"])

        # Build extension symlink commands from config
        symlink_commands = []
        ext_config = config.get("ext", {})

        for ext_name, ext_data in ext_config.items():
            # Only process extensions that are required by this runtime
            if ext_name in required_extensions and isinstance(ext_data, dict):
                is_sysext = ext_data.get("sysext", False)
                is_confext = ext_data.get("confext", False)

                symlink_commands.append(
                    f"""
OUTPUT_EXT=$AVOCADO_PREFIX/output/extensions/{ext_name}.raw
RUNTIMES_EXT=$VAR_DIR/lib/avocado/extensions/{ext_name}.raw
SYSEXT=$VAR_DIR/lib/extensions/{ext_name}.raw
CONFEXT=$VAR_DIR/lib/confexts/{ext_name}.raw

if [ -f "$OUTPUT_EXT" ]; then
    if ! cmp -s "$OUTPUT_EXT" "$RUNTIMES_EXT" 2>/dev/null; then
        ln -f $OUTPUT_EXT $RUNTIMES_EXT
    fi
else
    echo "Missing image for extension {ext_name}."
fi"""
                )

                if is_sysext:
                    cmd = f"ln -sf /var/lib/avocado/extensions/{
                        ext_name}.raw $SYSEXT"
                    symlink_commands.append(cmd)

                if is_confext:
                    cmd = f"ln -sf /var/lib/avocado/extensions/{
                        ext_name}.raw $CONFEXT"
                    symlink_commands.append(cmd)

        symlink_section = (
            "\n".join(symlink_commands)
            if symlink_commands
            else "# No extensions configured for symlinking"
        )

        script = f"""
VAR_DIR=$AVOCADO_PREFIX/runtimes/{runtime_name}/var-staging
mkdir -p "$VAR_DIR/lib/extensions"
mkdir -p "$VAR_DIR/lib/confexts"
mkdir -p "$VAR_DIR/lib/avocado/extensions"

DEPLOY_DIR="$AVOCADO_PREFIX/runtimes/{runtime_name}/deploy"
mkdir -p $DEPLOY_DIR

{symlink_section}

# Create btrfs image with extensions and confexts subvolumes
mkfs.btrfs -r "$VAR_DIR" \
    --subvol rw:lib/extensions \
    --subvol rw:lib/confexts \
    -f "$DEPLOY_DIR/avocado-image-var-avocado-{target}.var.img"

echo -e "\033[94m[INFO]\033[0m Running SDK lifecycle hook 'avocado-build' for '{target}'."
echo -e "  $(which avocado-build-{target})"
avocado-build-{target} {runtime_name}
"""

        return script

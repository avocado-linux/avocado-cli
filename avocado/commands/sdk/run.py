"""SDK run subcommand implementation."""
import os
import subprocess
import sys
import toml
from pathlib import Path
from avocado.commands.base import BaseCommand


class SdkRunCommand(BaseCommand):
    """Implementation of the 'sdk run' subcommand."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the sdk run subcommand's subparser."""
        run_parser = subparsers.add_parser("run", help="Run commands in SDK container")
        run_parser.add_argument("--config", "-c", required=True,
                               help="Path to config file (absolute or relative)")
        run_parser.add_argument("--interactive", "-i", action="store_true",
                               help="Drop into interactive shell in container")
        run_parser.add_argument("command", nargs="*",
                               help="Command and arguments to run in container")

    def _load_config(self, config_path):
        """Load and parse the TOML config file."""
        try:
            with open(config_path, 'r') as f:
                config = toml.load(f)
            return config
        except Exception as e:
            print(f"Error loading config file {config_path}: {e}")
            raise

    def _get_container_image(self, config):
        """Extract container image from config."""
        container_image = config.get('sdk', {}).get('image')
        if not container_image:
            raise ValueError("No container image specified in config under 'container.image'")
        return container_image

    def _setup_environment_vars(self):
        """Set up SDK environment variables (for container context)."""
        sdk_prefix = "/opt/avocado/sdk"
        sdk_sysroots = f"{sdk_prefix}/sysroots"

        env_vars = {
            "AVOCADO_SDK_PREFIX": sdk_prefix,
            "AVOCADO_SDK_SYSROOTS": sdk_sysroots,
            "DNF_SDK_HOST_PREFIX": sdk_prefix,
            "DNF_SDK_TARGET_PREFIX": f"{sdk_sysroots}/core2-64-avocado-linux",
        }

        # Set up DNF options
        dnf_sdk_host_opts = [
            f"--setopt=cachedir={sdk_prefix}/var/cache",
            f"--setopt=logdir={sdk_prefix}/var/log",
            f"--setopt=varsdir={sdk_prefix}/etc/dnf/vars",
            f"--setopt=persistdir={sdk_prefix}/var/lib/dnf",
            f"--setopt=reposdir={sdk_prefix}/etc/yum.repos.d",
        ]

        dnf_sdk_target_opts = [
            f"--setopt=varsdir={sdk_prefix}/etc/dnf/vars",
            f"--setopt=reposdir={sdk_prefix}/etc/yum.repos.d",
            "--setopt=tsflags=noscripts",
        ]

        env_vars["DNF_SDK_HOST_OPTS"] = " ".join(dnf_sdk_host_opts)
        env_vars["DNF_SDK_TARGET_OPTS"] = " ".join(dnf_sdk_target_opts)

        return env_vars

    def _create_container_script(self, user_command=None, interactive=False):
        """Create the script to run inside the container."""
        # Set up environment variables
        env_vars = self._setup_environment_vars()

        sdk_prefix = env_vars["AVOCADO_SDK_PREFIX"]
        sdk_sysroots = env_vars["AVOCADO_SDK_SYSROOTS"]

        script_lines = [
            "#!/bin/bash",
            "set -e",
            "",
            "# Set up SDK environment variables",
        ]

        # Export all environment variables
        for key, value in env_vars.items():
            script_lines.append(f'export {key}="{value}"')

        script_lines.extend([
            "",
            "# Create necessary directories",
            f"mkdir -p {sdk_sysroots}/sysext/{sdk_prefix}/var/lib",
            f"mkdir -p {sdk_sysroots}/confext/{sdk_prefix}/var/lib",
            "",
            "# Install SDK packages",
            "echo '--- Installing Avocado SDK packages ---'",
            "dnf check-update || true",  # Don't fail if updates available
            "dnf install -y avocado-sdk-qemux86-64",
            "",
            "# Install SDK toolchain",
            "echo '--- Installing Avocado SDK toolchain ---'",
            "dnf check-update $DNF_SDK_HOST_OPTS || true",
            "dnf install -y --setopt=tsflags=noscripts $DNF_SDK_HOST_OPTS avocado-sdk-toolchain",
            "",
            "# Source environment setup if it exists",
            f"if [ -f {sdk_prefix}/environment-setup ]; then",
            f"    echo 'Sourcing {sdk_prefix}/environment-setup'",
            f"    source {sdk_prefix}/environment-setup",
            "fi",
            "",
            "# Install target SDK packages",
            "echo '--- Installing target SDK packages ---'",
            f"dnf install -y $DNF_SDK_TARGET_OPTS --installroot={sdk_sysroots}/target-dev packagegroup-core-standalone-sdk-target",
            f"dnf install -y $DNF_SDK_TARGET_OPTS --installroot={sdk_sysroots}/rootfs packagegroup-avocado-rootfs",
            "",
            "# Copy RPM databases",
            f"if [ -d {sdk_sysroots}/rootfs/{sdk_prefix}/var/lib/rpm ]; then",
            f"    cp -rf {sdk_sysroots}/rootfs/{sdk_prefix}/var/lib/rpm {sdk_sysroots}/sysext/{sdk_prefix}/var/lib/",
            f"    cp -rf {sdk_sysroots}/rootfs/{sdk_prefix}/var/lib/rpm {sdk_sysroots}/confext/{sdk_prefix}/var/lib/",
            f"    echo 'Copied RPM databases'",
            "fi",
            "",
            "# Change to mounted working directory",
            "cd /opt",
            "",
        ])

        # notes for later:
        # bind mount cwd in as /opt
        # when we run we want to be the same user and group
        # go_su: does a switch user. toolchain install needs to be new user.
        # id -u
        # id -g
        # create user
        # create group
        # add to sudoers

        if interactive:
            script_lines.extend([
                "echo '--- Dropping into interactive shell ---'",
                "echo 'SDK environment is ready!'",
                f"echo 'SDK installed to: {sdk_prefix}'",
                "echo 'Working directory: /opt (mounted from host)'",
                "exec /bin/bash"
            ])
        elif user_command:
            script_lines.extend([
                "echo '--- Running user command ---'",
                f"exec {' '.join(user_command)}"
            ])
        else:
            script_lines.append("echo '--- SDK setup completed ---'")

        return "\n".join(script_lines)

    def _run_container(self, container_image, script_content, interactive=False):
        """Run the container with the generated script."""
        # Write script to temporary file
        script_path = "/tmp/sdk_setup_script.sh"
        with open(script_path, 'w') as f:
            f.write(script_content)
        os.chmod(script_path, 0o755)

        # Get current working directory
        cwd = os.getcwd()

        # Build container run command
        container_cmd = [
            "docker", "run",  # or "docker" depending on preference
            "--rm",           # Remove container after exit
            "-v", f"{script_path}:/sdk_setup_script.sh:ro",  # Mount script
            "-v", f"{cwd}:/opt",  # Mount current working directory as /opt
        ]

        # Add interactive flags if needed
        if interactive:
            container_cmd.extend(["-it"])

        # Add the container image and command
        container_cmd.extend([
            container_image,
            "/sdk_setup_script.sh"
        ])

        print(f"Running container: {' '.join(container_cmd)}")
        print(f"Mounting host directory: {cwd} -> /opt")
        try:
            result = subprocess.run(container_cmd)
            return result.returncode == 0
        except subprocess.CalledProcessError as e:
            print(f"Container execution failed: {e}")
            return False
        finally:
            # Clean up temporary script
            if os.path.exists(script_path):
                os.unlink(script_path)

    def execute(self, args, parser=None):
        """Execute the sdk run command."""
        try:
            # Validate arguments
            if args.interactive and args.command:
                print("Error: Cannot specify both --interactive and a command")
                return False

            if not args.interactive and not args.command:
                print("Error: Must specify either --interactive or a command to run")
                return False

            # Load config
            config = self._load_config(args.config)
            container_image = self._get_container_image(config)
            print(f"Using container image: {container_image}")

            # Create and run container script
            script_content = self._create_container_script(
                user_command=args.command if not args.interactive else None,
                interactive=args.interactive
            )

            success = self._run_container(container_image, script_content, args.interactive)

            if success:
                print("--- Container execution completed successfully! ---")
                return True
            else:
                print("--- Container execution failed ---")
                return False

        except Exception as e:
            print(f"Unexpected error: {e}")
            return False

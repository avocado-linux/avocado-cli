"""Shared container utility for SDK container operations."""
import os
import shlex
import subprocess
import sys
from typing import List, Dict, Optional, Union
from avocado.utils.output import print_error


class ContainerRunner:
    """Utility class for running SDK containers with shared logic."""

    def __init__(self, container_tool: str = "podman", verbose: bool = False):
        """Initialize the container runner.

        Args:
            container_tool: Container tool to use (defaults to "podman")
            verbose: Whether to print container commands before execution
        """
        self.container_tool = container_tool
        self.cwd = os.getcwd()
        self.verbose = verbose

    def run_container_command(
        self,
        container_image: str,
        command: Union[str, List[str]],
        target: Optional[str] = None,
        env_vars: Optional[Dict[str, str]] = None,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        interactive: bool = False,
        tty: bool = True,
        additional_volumes: Optional[List[str]] = None,
    ) -> bool:
        """Run a command in an SDK container with entrypoint setup.

        Args:
            container_image: Container image to use
            command: Command to run after entrypoint setup (string or list)
            target: Target architecture (sets AVOCADO_SDK_TARGET env var)
            env_vars: Additional environment variables to set
            container_name: Name to assign to the container
            detach: Run container in background
            rm: Remove container when it exits
            interactive: Keep STDIN open for interactive mode
            tty: Allocate a pseudo-TTY
            additional_volumes: Additional volume mounts beyond the defaults

        Returns:
            bool: True if container ran successfully, False otherwise
        """
        try:
            container_cmd = self._build_container_command(
                container_image=container_image,
                command=command,
                target=target,
                env_vars=env_vars,
                container_name=container_name,
                detach=detach,
                rm=rm,
                interactive=interactive,
                tty=tty,
                additional_volumes=additional_volumes,
            )

            return self._execute_container_command(container_cmd, detach)

        except Exception as e:
            print_error(f"Container execution failed: {e}")
            return False

    def _build_container_command(
        self,
        container_image: str,
        command: Union[str, List[str]],
        target: Optional[str] = None,
        env_vars: Optional[Dict[str, str]] = None,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        interactive: bool = False,
        tty: bool = True,
        additional_volumes: Optional[List[str]] = None,
    ) -> List[str]:
        """Build the complete container command."""
        container_cmd = [self.container_tool, "run"]

        # Container options
        if rm:
            container_cmd.append("--rm")
        if container_name:
            container_cmd.extend(["--name", container_name])
        if detach:
            container_cmd.append("-d")

        # TTY and stdin handling
        if interactive and not detach:
            container_cmd.extend(["-i", "-t"])
        elif tty:
            container_cmd.append("-t")

        # Default volume mounts
        container_cmd.extend([
            "-v", f"{self.cwd}:/opt:z"
        ])

        # Additional volume mounts
        if additional_volumes:
            for volume in additional_volumes:
                container_cmd.extend(["-v", volume])

        # Environment variables
        if target:
            container_cmd.extend(["-e", f"AVOCADO_SDK_TARGET={target}"])

        if env_vars:
            for key, value in env_vars.items():
                container_cmd.extend(["-e", f"{key}={value}"])

        # Add the container image
        container_cmd.append(container_image)

        # Add the command to execute
        if isinstance(command, str):
            container_cmd.append(command)
        else:
            container_cmd.extend(command)

        return container_cmd

    def _execute_container_command(self, container_cmd: List[str], detach: bool = False) -> bool:
        """Execute the container command and handle the result."""
        try:
            if self.verbose:
                print(f"Mounting host directory: {self.cwd} -> /opt\n")
                print(f"Container command: {' '.join(container_cmd)}")

            if detach:
                result = subprocess.run(
                    container_cmd, check=True, capture_output=True, text=True)
                container_id = result.stdout.strip()
                print(f"Container started in detached mode with ID: {
                      container_id}")
                return True
            else:
                result = subprocess.run(container_cmd, check=False)
                return result.returncode == 0

        except KeyboardInterrupt:
            print(
                "\nINFO: Keyboard interrupt received. Container process may also be stopping.")
            return False
        except subprocess.CalledProcessError as e:
            print_error(f"Container execution failed: {e}")
            if hasattr(e, 'stdout') and e.stdout:
                print(f"STDOUT: {e.stdout}", file=sys.stderr)
            if hasattr(e, 'stderr') and e.stderr:
                print(f"STDERR: {e.stderr}", file=sys.stderr)
            return False
        except FileNotFoundError:
            print_error(f"{
                self.container_tool} command not found. Is it installed and in your PATH?")
            return False


class SdkContainerHelper:
    """Helper class for common SDK container operations."""

    def __init__(self, container_runner: Optional[ContainerRunner] = None, verbose: bool = False):
        """Initialize the SDK container helper.

        Args:
            container_runner: Container runner instance to use
            verbose: Whether to enable verbose output for container commands
        """
        self.runner = container_runner or ContainerRunner(verbose=verbose)

    def _create_entrypoint_script(self, target: str, source_environment: bool = True) -> str:
        """Create the embedded entrypoint script content.

        Args:
            target: Target architecture
            source_environment: Whether to source the environment setup at the end
        """
        script = f'''
set -e

# Get codename from environment or os-release
if [ -n "$AVOCADO_SDK_CODENAME" ]; then
    CODENAME="$AVOCADO_SDK_CODENAME"
else
    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        CODENAME=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    CODENAME=${{CODENAME:-dev}}
fi

export AVOCADO_SDK_PREFIX="/opt/_avocado/sdk"
export AVOCADO_SDK_SYSROOTS="${{AVOCADO_SDK_PREFIX}}/sysroots"
export DNF_SDK_HOST_PREFIX="${{AVOCADO_SDK_PREFIX}}"
export DNF_SDK_HOST="dnf --setopt=varsdir=${{DNF_SDK_HOST_PREFIX}}/etc/dnf/vars --setopt=reposdir=${{DNF_SDK_HOST_PREFIX}}/etc/yum.repos.d --releasever=$CODENAME --best --setopt=tsflags=noscripts"

export DNF_SDK_HOST_OPTS="--setopt=cachedir=${{DNF_SDK_HOST_PREFIX}}/var/cache --setopt=logdir=${{DNF_SDK_HOST_PREFIX}}/var/log --setopt=persistdir=${{DNF_SDK_HOST_PREFIX}}/var/lib/dnf"

export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"
export RPM_NO_CHROOT_FOR_SCRIPTS=1

if [ ! -f "${{AVOCADO_SDK_PREFIX}}/environment-setup" ]; then
    echo "Initializing Avocado SDK"
    mkdir -p $AVOCADO_SDK_PREFIX/var/lib
    cp -r /var/lib/rpm $AVOCADO_SDK_PREFIX/var/lib/
    cp -r /var/cache $AVOCADO_SDK_PREFIX/var/cache/

    mkdir -p $AVOCADO_SDK_PREFIX/etc
    cp /etc/rpmrc $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/rpm $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/dnf $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/yum.repos.d $AVOCADO_SDK_PREFIX/etc

    mkdir -p $AVOCADO_SDK_PREFIX/usr/lib/rpm
    cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/usr/lib/rpm/

    # Before calling DNF, $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros needs to be updated to point:
    #   - /usr -> $AVOCADO_SDK_PREFIX/usr
    #   - /var -> $AVOCADO_SDK_PREFIX/var
    sed -i 's|^%_usr[[:space:]]*/usr$|%_usr                   /opt/_avocado/sdk/usr|' $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros
    sed -i 's|^%_var[[:space:]]*/var$|%_var                   /opt/_avocado/sdk/var|' $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" $DNF_SDK_HOST $DNF_SDK_HOST_OPTS -y install "avocado-sdk-{target}"
    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" $DNF_SDK_HOST $DNF_SDK_HOST_OPTS check-update || true
    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" $DNF_SDK_HOST $DNF_SDK_HOST_OPTS -y install avocado-sdk-toolchain

    echo "Installing target-dev sysroot."
    $DNF_SDK_HOST -y --installroot ${{AVOCADO_SDK_SYSROOTS}}/target-dev install packagegroup-core-standalone-sdk-target
    echo "Installing rootfs sysroot."
    $DNF_SDK_HOST -y --installroot ${{AVOCADO_SDK_SYSROOTS}}/rootfs install avocado-pkg-rootfs
fi

cd /opt
'''

        if source_environment:
            script += '''
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi
'''

        return script

    def run_dnf_install(
        self,
        container_image: str,
        packages: List[str],
        target: str,
        installroot: Optional[str] = None,
        dnf_flags: str = "",
        env_vars: Optional[Dict[str, str]] = None,
        verbose: bool = False,
        source_environment: bool = True
    ) -> bool:
        """Run DNF install command in SDK container.

        Args:
            container_image: Container image to use
            packages: List of packages to install
            target: Target architecture
            installroot: Optional installroot for DNF (e.g., for extensions)
            dnf_flags: Additional DNF command flags
            env_vars: Additional environment variables
            verbose: Whether to print container command before execution
            source_environment: Whether to source the SDK environment setup

        Returns:
            bool: True if installation succeeded, False otherwise
        """
        # Get the entrypoint script content
        entrypoint_script = self._create_entrypoint_script(
            target, source_environment)

        # Build the DNF command
        dnf_install_cmd = f"$DNF_SDK_HOST {dnf_flags}"

        if installroot:
            dnf_install_cmd += f" --installroot={installroot}"

        dnf_install_cmd += f" install {' '.join(packages)}"

        # Create the complete bash command that includes entrypoint logic + DNF execution
        bash_cmd = [
            "bash", "-c",
            entrypoint_script +
            f"\n# Execute DNF install command\n{dnf_install_cmd}"
        ]

        # If verbose is specified for this call, create a new runner with verbose enabled
        runner = self.runner
        if verbose and not self.runner.verbose:
            runner = ContainerRunner(
                container_tool=self.runner.container_tool, verbose=True)

        return runner.run_container_command(
            container_image=container_image,
            command=bash_cmd,
            target=target,
            env_vars=env_vars
        )

    def run_interactive_shell(
        self,
        container_image: str,
        target: str,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        env_vars: Optional[Dict[str, str]] = None,
        verbose: bool = False,
        source_environment: bool = True
    ) -> bool:
        """Run an interactive shell in SDK container.

        Args:
            container_image: Container image to use
            target: Target architecture
            container_name: Optional container name
            detach: Run in detached mode
            rm: Remove container when it exits
            env_vars: Additional environment variables
            verbose: Whether to print container command before execution
            source_environment: Whether to source the SDK environment setup

        Returns:
            bool: True if shell ran successfully, False otherwise
        """
        # Get the entrypoint script content
        entrypoint_script = self._create_entrypoint_script(
            target, source_environment)

        # Create bash command that runs entrypoint setup then drops to interactive shell
        bash_cmd = [
            "bash", "-c",
            entrypoint_script + "\n# Drop to interactive shell\nexec bash"
        ]

        # If verbose is specified for this call, create a new runner with verbose enabled
        runner = self.runner
        if verbose and not self.runner.verbose:
            runner = ContainerRunner(
                container_tool=self.runner.container_tool, verbose=True)

        return runner.run_container_command(
            container_image=container_image,
            command=bash_cmd,
            target=target,
            container_name=container_name,
            detach=detach,
            rm=rm,
            interactive=True,
            env_vars=env_vars
        )

    def run_user_command(
        self,
        container_image: str,
        command: List[str],
        target: str,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        env_vars: Optional[Dict[str, str]] = None,
        verbose: bool = False,
        source_environment: bool = True
    ) -> bool:
        """Run a user-specified command in SDK container.

        Args:
            container_image: Container image to use
            command: User command to run
            target: Target architecture
            container_name: Optional container name
            detach: Run in detached mode
            rm: Remove container when it exits
            env_vars: Additional environment variables
            verbose: Whether to print container command before execution
            source_environment: Whether to source the SDK environment setup

        Returns:
            bool: True if command ran successfully, False otherwise
        """
        # Get the entrypoint script content
        entrypoint_script = self._create_entrypoint_script(
            target, source_environment)

        # Escape and join the user command
        escaped_command = ' '.join(shlex.quote(arg) for arg in command)

        # Create bash command that runs entrypoint setup then executes user command
        bash_cmd = [
            "bash", "-c",
            entrypoint_script +
            f"\n# Execute user command\nexec {escaped_command}"
        ]

        # If verbose is specified for this call, create a new runner with verbose enabled
        runner = self.runner
        if verbose and not self.runner.verbose:
            runner = ContainerRunner(
                container_tool=self.runner.container_tool, verbose=True)

        return runner.run_container_command(
            container_image=container_image,
            command=bash_cmd,
            target=target,
            container_name=container_name,
            detach=detach,
            rm=rm,
            env_vars=env_vars
        )
